import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { DEFAULTS, presetFor, QUALITY_PRESETS, type Settings } from "./settings";
import { Icon } from "./icons";
import "./theme.css";
import "./App.css";
const CATEGORIES = [
  { id: "system", label: "系统" },
  { id: "capture", label: "截图" },
  { id: "pin", label: "贴图" },
  { id: "clipboard", label: "剪贴板" },
  { id: "save", label: "保存" },
  { id: "annotate", label: "标注" },
  { id: "hotkeys", label: "快捷键/动作" },
  { id: "builtin", label: "内置快捷键" },
  { id: "record", label: "录制" },
  { id: "ocr", label: "OCR" },
  { id: "about", label: "关于" },
] as const;

type CategoryId = (typeof CATEGORIES)[number]["id"];

const FPS_CHOICES = [15, 24, 30, 60];
const PALETTE = ["#ef4444", "#f59e0b", "#22c55e", "#3b82f6", "#a855f7", "#1a1a1a"];

// ---- 通用控件（浅色卡片风格，docs/UI设计规范.md §5）----

function Row(props: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div className="set-row">
      <div className="set-label">
        <div>{props.label}</div>
        {props.hint && <div className="muted small">{props.hint}</div>}
      </div>
      <div className="set-control">{props.children}</div>
    </div>
  );
}

function Toggle(props: { value: boolean; onChange: (v: boolean) => void }) {
  return (
    <button
      className={props.value ? "toggle on" : "toggle"}
      onClick={() => props.onChange(!props.value)}
      aria-pressed={props.value}
    >
      <span className="knob" />
    </button>
  );
}

function Select<T extends string | number>(props: {
  value: T;
  choices: readonly T[];
  labels?: Record<string, string>;
  onChange: (v: T) => void;
}) {
  return (
    <select
      className="set-select"
      value={String(props.value)}
      onChange={(e) => {
        const raw = e.target.value;
        const v = (typeof props.value === "number" ? Number(raw) : raw) as T;
        props.onChange(v);
      }}
    >
      {props.choices.map((c) => (
        <option key={String(c)} value={String(c)}>
          {props.labels?.[String(c)] ?? String(c)}
        </option>
      ))}
    </select>
  );
}

// 导航图标已并入共享模块 icons.tsx（nav- 前缀：与 capture/pin/save/ocr/clipboard 同名但字形不同，
// 无前缀会覆盖成别的图）。path 逐字保留，16px / 1.6 描边口径不变。
function NavIcon(props: { id: CategoryId }) {
  return (
    <span className="nav-ico" aria-hidden>
      <Icon name={`nav-${props.id}`} size={16} strokeWidth={1.6} />
    </span>
  );
}


// ---- 可编辑热键页（点击进入捕获模式，写 hotkeys[action]）----

// 内置默认热键：hotkeys[action] 为空/缺失时的回退值
const HOTKEY_ACTIONS = [
  { id: "capture", name: "截图", def: "Ctrl+1" },
  { id: "pin", name: "贴图", def: "Ctrl+2" },
  { id: "record", name: "录屏（开始/停止）", def: "Ctrl+Shift+R" },
  // 暂停/继续录制：默认不占键（def 为空），想用就自己绑——默认绑全局键会抢别的程序。
  { id: "record_pause", name: "录屏（暂停/继续）", def: "" },
  { id: "clip", name: "剪贴板历史", def: "Ctrl+Shift+V" },
] as const;

// ---- 热键组合的生成与展示（2026-09-24「剪贴板快捷键不能更换」根因修复）----
// 主键名必须用 e.code 物理键名切出与后端 hotkeys::parse_code 对齐的键名：
// e.key 受 Shift/布局污染（Shift+/ 产 "?"），符号键值后端解析不了 → 注册失败
// 回退默认 = 用户按新键没反应。Numpad/F区/标点在后端 parse_code 均已登记。
function mainKeyFromEvent(e: KeyboardEvent): string | null {
  const c = e.code;
  if (!c) {
    // 极老内核无 code：退回 e.key（仅字母数字可靠），保证不比旧行为差。
    const k = e.key;
    if (k === " ") return "Space";
    return k.length === 1 ? k.toUpperCase() : k;
  }
  if (/^Key[A-Z]$/.test(c)) return c.slice(3);
  if (/^Digit\d$/.test(c)) return c.slice(5);
  if (/^F\d{1,2}$/.test(c)) return c;
  if (/^Numpad\d$/.test(c)) return c; // "Numpad5" → 后端 "numpad5"
  return c in NAMED_MAIN_KEYS ? c : null;
}

// 静态「命名键 e.code → 后端 parse_code 键名」白名单（键名单字面量表 → Record）。
const NAMED_MAIN_KEYS: Record<string, true> = {
  Space: true, Enter: true, Tab: true, Escape: true, Backspace: true,
  Delete: true, Insert: true, Home: true, End: true,
  PageUp: true, PageDown: true, ArrowUp: true, ArrowDown: true,
  ArrowLeft: true, ArrowRight: true, PrintScreen: true, Minus: true,
  Equal: true, Backquote: true, BracketLeft: true, BracketRight: true,
  Backslash: true, Semicolon: true, Quote: true, Comma: true,
  Period: true, Slash: true, NumpadAdd: true, NumpadSubtract: true,
  NumpadMultiply: true, NumpadDivide: true,
};

// 配置值/事件键名 → 人类可读（"BracketLeft+Shift" → "[ + Shift"；中文化常用键）。
const KEY_LABEL: Record<string, string> = {
  Space: "空格", Enter: "回车", Escape: "Esc", Tab: "Tab", Backspace: "退格",
  Delete: "Del", Insert: "Ins", Home: "Home", End: "End",
  PageUp: "PgUp", PageDown: "PgDn",
  ArrowUp: "↑", ArrowDown: "↓", ArrowLeft: "←", ArrowRight: "→",
  PrintScreen: "PrtSc",
  Minus: "-", Equal: "=", Backquote: "`",
  BracketLeft: "[", BracketRight: "]", Backslash: "\\",
  Semicolon: ";", Quote: "'", Comma: ",", Period: ".", Slash: "/",
  NumpadAdd: "小键盘+", NumpadSubtract: "小键盘-",
  NumpadMultiply: "小键盘×", NumpadDivide: "小键盘/",
};
function fmtKey(combo: string): string {
  return combo
    .split("+")
    .map((p) => KEY_LABEL[p] ?? (/^Numpad\d$/.test(p) ? `小键盘${p.slice(6)}` : p))
    .join(" + ");
}

function HotkeyEditor(props: {
  hotkeys: Record<string, string>;
  onSave: (action: string, combo: string | null) => void;
}) {
  const [capturing, setCapturing] = useState<string | null>(null);
  const [conflict, setConflict] = useState<string>("");

  // 某动作的生效热键：优先用户覆盖，否则内置默认
  const effective = useCallback(
    (id: string) =>
      props.hotkeys[id] || HOTKEY_ACTIONS.find((a) => a.id === id)!.def,
    [props.hotkeys],
  );

  // 捕获模式：监听下一个按键组合，Esc 取消，冲突则提示不落盘
  useEffect(() => {
    if (!capturing) return;
    const handler = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === "Escape") {
        setCapturing(null);
        setConflict("");
        return;
      }
      // 仅按下修饰键时继续等待主键
      if (["Control", "Shift", "Alt", "Meta"].includes(e.key)) return;
      const main = mainKeyFromEvent(e);
      if (main === null) {
        setConflict(`不支持的按键（${e.code || e.key}），请换一个`);
        return;
      }
      const mods: string[] = [];
      if (e.ctrlKey) mods.push("Ctrl");
      if (e.shiftKey) mods.push("Shift");
      if (e.altKey) mods.push("Alt");
      if (e.metaKey) mods.push("Win");
      const combo = [...mods, main].join("+");
      // 冲突检测：其它动作已占用该组合
      const clash = HOTKEY_ACTIONS.find(
        (a) =>
          a.id !== capturing &&
          effective(a.id).toLowerCase() === combo.toLowerCase(),
      );
      if (clash) {
        setConflict(`与「${clash.name}」冲突：${fmtKey(combo)}，请换一个`);
        return;
      }
      props.onSave(capturing, combo);
      setCapturing(null);
      setConflict("");
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [capturing, effective, props]);

  return (
    <>
      <h1>快捷键/动作</h1>
      <p className="muted small">
        点击右侧热键进入捕获模式，按下新的组合键即生效；按 Esc 取消。留空则使用内置默认。
      </p>
      <div className="hk-list">
        {HOTKEY_ACTIONS.map((a) => {
          const isCap = capturing === a.id;
          const overridden = !!props.hotkeys[a.id];
          return (
            <div className="hk-row" key={a.id}>
              <span className="hk-name">{a.name}</span>
              <span className="hk-actions">
                {overridden && !isCap && (
                  <button
                    className="hk-reset"
                    onClick={() => props.onSave(a.id, null)}
                    title="恢复内置默认"
                  >
                    恢复默认
                  </button>
                )}
                <span
                  className={isCap ? "hk-key capturing" : "hk-key editable"}
                  onClick={() => {
                    setConflict("");
                    setCapturing(isCap ? null : a.id);
                  }}
                >
                  {isCap ? "按下组合键…（Esc 取消）" : fmtKey(effective(a.id))}
                </span>
              </span>
            </div>
          );
        })}
      </div>
      {conflict && <div className="hk-conflict">{conflict}</div>}
    </>
  );
}

function App() {
  const [active, setActive] = useState<CategoryId>("system");
  const [settings, setSettings] = useState<Settings>(DEFAULTS);
  const [loaded, setLoaded] = useState(false);
  const [savedAt, setSavedAt] = useState<string>("");
  const [httpOk, setHttpOk] = useState<string>("检测中…");
  const [audioDevices, setAudioDevices] = useState<string[]>([]);

  // 录制页的音频设备候选：只在切到该页时拉一次（枚举 dshow 设备要跑一次 ffmpeg，不常驻）。
  useEffect(() => {
    if (active !== "record") return;
    invoke<string[]>("list_audio_devices")
      .then((d) => setAudioDevices(Array.isArray(d) ? d : []))
      .catch((e) => console.error("list_audio_devices 失败", e));
  }, [active]);

  // 挂载拉设置；失败用 DEFAULTS 且明确提示（不静默假通过）。
  useEffect(() => {
    invoke<Settings>("get_settings")
      .then((s) => {
        setSettings({ ...DEFAULTS, ...s });
        setLoaded(true);
      })
      .catch((e) => {
        console.error("get_settings 失败", e);
        setLoaded(false);
      });
  }, []);

  // 关于页探活必须打**配置里的端口**：用户改过 config 的 port 后，写死 47821 会一直
  // 误报「未连接」。依赖 settings.port，所以 get_settings 回来（端口与默认值不同时）
  // 会自动用真端口重探一次；连不上时把探测目标写进文案，便于用户判断是不是端口不对。
  useEffect(() => {
    let cancelled = false;
    const port = settings.port;
    setHttpOk("检测中…");
    fetch(`http://127.0.0.1:${port}/v1/health`)
      .then((r) => r.json())
      .then((j) => {
        if (!cancelled) setHttpOk(`已就绪 · v${j.version} · 127.0.0.1:${port}`);
      })
      .catch(() => {
        if (!cancelled) setHttpOk(`未连接（127.0.0.1:${port} 无响应）`);
      });
    return () => {
      cancelled = true;
    };
  }, [settings.port]);

  const save = useCallback((next: Settings) => {
    setSettings(next);
    invoke<Settings>("update_settings", { newCfg: next })
      .then((finalCfg) => {
        setSettings({ ...DEFAULTS, ...finalCfg });
        setSavedAt(new Date().toLocaleTimeString());
      })
      .catch((e) => console.error("update_settings 失败", e));
  }, []);

  const patch = useCallback(
    (p: Partial<Settings>) => save({ ...settings, ...p }),
    [save, settings],
  );

  // 保存目录「浏览…」：后端弹系统对话框，返回选中绝对路径，取消则 null
  const browseSaveDir = useCallback(() => {
    invoke<string | null>("pick_save_path", { defaultName: "", kind: "png" })
      .then((picked) => {
        if (picked) patch({ save_dir: picked });
      })
      .catch((e) => console.error("pick_save_path 失败", e));
  }, [patch]);

  // 导出剪贴板历史（P1 #11）：先选目录（空名=选目录分支），后端复制 json+图片。
  const exportClipHistory = useCallback(() => {
    invoke<string | null>("pick_save_path", { defaultName: "", kind: "json" })
      .then((picked) => {
        if (picked) return invoke("clip_export", { dest: picked });
      })
      .catch((e) => console.error("导出剪贴板历史失败", e));
  }, []);

  const resetAll = useCallback(() => {
    invoke<Settings>("reset_settings")
      .then((finalCfg) => {
        setSettings({ ...DEFAULTS, ...finalCfg });
        setSavedAt(new Date().toLocaleTimeString());
        // reset_settings 会清空 hotkeys，需重注册回内置默认键
        return invoke("re_register_hotkeys");
      })
      .catch((e) => console.error("reset_settings 失败", e));
  }, []);

  // 重绑热键：先落盘 update_settings，再 re_register_hotkeys 生效并**回读注册结果**
  // ——后端会如实返回 requested vs effective + 失败原因（注册失败曾静默回退默认键，
  // 用户 2026-09-24 实报「剪贴板快捷键不能更换」的根因之一），失败红字显示不许静默。
  const [hkFeedback, setHkFeedback] = useState<{ ok: boolean; msg: string } | null>(null);
  const saveHotkey = useCallback(
    (action: string, combo: string | null) => {
      const nextHotkeys = { ...settings.hotkeys };
      if (combo === null) delete nextHotkeys[action];
      else nextHotkeys[action] = combo;
      const next = { ...settings, hotkeys: nextHotkeys };
      setSettings(next);
      invoke<Settings>("update_settings", { newCfg: next })
        .then((finalCfg) => {
          setSettings({ ...DEFAULTS, ...finalCfg });
          setSavedAt(new Date().toLocaleTimeString());
          return invoke<
            { action: string; requested: string; effective: string; ok: boolean; error: string | null }[]
          >("re_register_hotkeys");
        })
        .then((res) => {
          const name = HOTKEY_ACTIONS.find((a) => a.id === action)?.name ?? action;
          const mine = Array.isArray(res) ? res.find((r) => r.action === action) : undefined;
          if (!mine) {
            setHkFeedback(null);
          } else if (!mine.ok) {
            const want = combo ? `「${fmtKey(combo)}」` : "默认键";
            setHkFeedback({
              ok: false,
              msg: `${name}：${want}未能生效（${mine.error ?? "注册失败"}），当前无可用热键`,
            });
          } else if (mine.requested && mine.effective !== mine.requested) {
            setHkFeedback({
              ok: false,
              msg: `${name}：「${fmtKey(mine.requested)}」注册失败（${mine.error ?? "冲突"}），已回退为 ${fmtKey(mine.effective)}`,
            });
          } else {
            setHkFeedback({
              ok: true,
              msg: `${name}：${fmtKey(mine.effective)} 已生效`,
            });
          }
        })
        .catch((e) => {
          console.error("热键重绑失败", e);
          setHkFeedback({ ok: false, msg: `热键重绑失败：${String(e)}` });
        });
    },
    [settings],
  );

  if (!loaded) {
    return (
      <div className="settings">
        <main className="content" style={{ gridColumn: "1 / -1" }}>
          <h1>SnapPin 配置</h1>
          <p className="muted">设置加载中…（若长期无响应说明后端命令异常，看控制台）</p>
        </main>
      </div>
    );
  }

  const s = settings;

  return (
    <div className="settings">
      <nav className="nav">
        <div className="nav-title">SnapPin 配置</div>
        <ul>
          {CATEGORIES.map((c) => (
            <li
              key={c.id}
              className={c.id === active ? "nav-item active" : "nav-item"}
              onClick={() => setActive(c.id)}
            >
              <NavIcon id={c.id} />
              <span>{c.label}</span>
            </li>
          ))}
        </ul>
        <button className="reset-btn" onClick={resetAll}>
          恢复所有默认设置
        </button>
      </nav>

      <main className="content">
        {savedAt && (
          <div className="saved-tip muted small">已保存 · {savedAt}</div>
        )}
        {/* 热键注册结果反馈：失败/回退红字、生效绿字——注册结果必须可见不许静默 */}
        {hkFeedback && (
          <div
            className="saved-tip small"
            style={{ color: hkFeedback.ok ? "var(--accent)" : "var(--danger)" }}
          >
            {hkFeedback.msg}
          </div>
        )}

        {active === "system" && (
          <>
            <h1>系统</h1>
            <Row label="开机自启" hint="登录后自动常驻托盘（重启应用生效）">
              <Toggle value={s.auto_start} onChange={(v) => patch({ auto_start: v })} />
            </Row>
            <Row label="关闭到托盘" hint="点关闭按钮时隐藏到托盘，而非退出程序">
              <Toggle
                value={s.close_to_tray}
                onChange={(v) => patch({ close_to_tray: v })}
              />
            </Row>
            <Row label="以管理员运行" hint="下次重启时以管理员权限启动（用于全局热键在提权窗口生效）">
              <Toggle
                value={s.run_as_admin}
                onChange={(v) => patch({ run_as_admin: v })}
              />
            </Row>
            <Row
              label="桌面悬浮球"
              hint="常驻置顶小圆球：点它截图，拖它移动，右键隐藏（托盘菜单里也能开关）"
            >
              <Toggle
                value={s.desktop_ball}
                onChange={(v) => {
                  patch({ desktop_ball: v });
                  void invoke(v ? "open_ball" : "close_ball").catch(() => {});
                }}
              />
            </Row>
          </>
        )}

        {active === "capture" && (
          <>
            <h1>截图</h1>
            <Row label="截图后自动复制" hint="完成截图后图片进剪贴板">
              <Toggle
                value={s.capture_copy_after}
                onChange={(v) => patch({ capture_copy_after: v })}
              />
            </Row>
            <Row label="截图后自动保存" hint="完成截图后落文件（目录见「保存」）">
              <Toggle
                value={s.capture_auto_save}
                onChange={(v) => patch({ capture_auto_save: v })}
              />
            </Row>
            <Row
              label="保存前确认（可选路径 / 快速保存）"
              hint="开启：截图完成弹保存对话框，可自选路径；关闭：直接快速保存到「保存」页的目录"
            >
              <Toggle
                value={s.capture_confirm_save}
                onChange={(v) => patch({ capture_confirm_save: v })}
              />
            </Row>
            <Row label="智能窗口检测" hint="框选前高亮鼠标下的窗口（滚轮换层级后续增强）">
              <Toggle
                value={s.capture_ui_detect}
                onChange={(v) => patch({ capture_ui_detect: v })}
              />
            </Row>
            <Row label="显示尺寸标签" hint="选区左上角实时显示 x,y 与宽高像素">
              <Toggle
                value={s.capture_show_size_tag}
                onChange={(v) => patch({ capture_show_size_tag: v })}
              />
            </Row>
            <Row
              label="长截图滚动步长"
              hint="长截图每步滚轮档数（1–5）：越大每步跳得越远，跳太远容易漏内容"
            >
              <input
                className="set-input"
                type="number"
                min={1}
                max={5}
                step={1}
                value={s.longshot_scroll_notches}
                onChange={(e) =>
                  patch({
                    longshot_scroll_notches: Math.min(
                      5,
                      Math.max(1, Number(e.target.value) || 1),
                    ),
                  })
                }
              />
            </Row>
            <Row
              label="长截图每步等待上限"
              hint="毫秒（100–3000）：等画面渲染落定就进入下一步，所以这是「最慢速度」；页面重、动画慢就调大"
            >
              <input
                className="set-input"
                type="number"
                min={100}
                max={3000}
                step={50}
                value={s.longshot_settle_ms}
                onChange={(e) =>
                  patch({
                    longshot_settle_ms: Math.min(
                      3000,
                      Math.max(100, Number(e.target.value) || 400),
                    ),
                  })
                }
              />
            </Row>
          </>
        )}

        {active === "pin" && (
          <>
            <h1>贴图</h1>
            <Row label={`默认不透明度（${s.pin_opacity}%）`}>
              <input
                type="range"
                min={1}
                max={100}
                value={s.pin_opacity}
                onChange={(e) => patch({ pin_opacity: Number(e.target.value) })}
              />
            </Row>
            <Row label="滚轮缩放" hint="贴图窗口上滚动鼠标滚轮放大/缩小">
              <Toggle value={s.pin_wheel_zoom} onChange={(v) => patch({ pin_wheel_zoom: v })} />
            </Row>
            <Row label="窗口阴影">
              <Toggle value={s.pin_shadow} onChange={(v) => patch({ pin_shadow: v })} />
            </Row>
          </>
        )}

        {active === "clipboard" && (
          <>
            <h1>剪贴板</h1>
            <Row
              label="记录剪贴板历史"
              hint="关闭后不再记录新复制的内容；已有历史保留，可随时清空"
            >
              <Toggle
                value={s.clip_enabled}
                onChange={(v) => patch({ clip_enabled: v })}
              />
            </Row>
            <Row
              label="历史条数上限"
              hint="超出后自动删除最旧条目（含其图片文件）；范围 10–1000"
            >
              <input
                className="set-input"
                type="number"
                min={10}
                max={1000}
                step={10}
                value={s.clip_max}
                onChange={(e) =>
                  patch({
                    clip_max: Math.min(
                      1000,
                      Math.max(10, Number(e.target.value) || 100),
                    ),
                  })
                }
              />
            </Row>
            <Row
              label="打开面板热键"
              hint="在「快捷键/动作」页的「剪贴板历史」行修改；改完下方会显示生效结果"
            >
              {/* 显示**实际生效键**而非写死「默认」——改过之后这行不能再撒谎 */}
              <span className="muted small">
                {fmtKey(s.hotkeys.clip || "Ctrl+Shift+V")}
                {s.hotkeys.clip ? "" : "（默认）"}
              </span>
            </Row>
            {/* ---- P1 高级功能（2026-09-24 十三项规格）---- */}
            <Row
              label="忽略来源应用"
              hint="这些应用里复制的内容不入库；留空 = 全部记录"
            >
              {/* 进程名多行文本（P1 先用多行文本兜底） */}
              <textarea
                className="set-input"
                rows={3}
                placeholder="每行一个进程名，如 notepad.exe"
                value={s.clip_ignore_apps}
                onChange={(e) => patch({ clip_ignore_apps: e.target.value })}
              />
            </Row>
            <Row label="跳过敏感内容" hint="自动识别私钥、服务 Token、AWS Key、JWT 等高置信机密，不写入历史">
              <Toggle value={s.clip_skip_sensitive} onChange={(v) => patch({ clip_skip_sensitive: v })} />
            </Row>
            <Row
              label="面板激活后聚焦搜索框"
              hint="热键唤出面板不再抢走原窗口的插入符；点了面板或按 Ctrl+F 后，搜索框接收输入"
            >
              <Toggle value={s.clip_search_focus} onChange={(v) => patch({ clip_search_focus: v })} />
            </Row>
            <Row label="关闭面板时清空搜索词" hint="下次打开显示完整历史">
              <Toggle value={s.clip_search_clear_on_hide} onChange={(v) => patch({ clip_search_clear_on_hide: v })} />
            </Row>
            <Row label="点击条目直接粘贴" hint="写回剪贴板后自动粘进上个窗口；关掉则仅写回，需手动 Ctrl+V">
              <Toggle value={s.clip_auto_paste} onChange={(v) => patch({ clip_auto_paste: v })} />
            </Row>
            <Row label="粘贴后自动收起面板" hint="关掉则面板留着供连续粘贴，由你再按热键/✕ 收起">
              <Toggle
                value={s.clip_close_after_paste}
                onChange={(v) => patch({ clip_close_after_paste: v })}
              />
            </Row>
            <Row label="粘贴触发方式" hint="单击 / 双击（仅当上一项开启）">
              <Select<"single" | "double">
                value={s.clip_paste_trigger}
                choices={["single", "double"] as const}
                labels={{ single: "单击", double: "双击" }}
                onChange={(v) => patch({ clip_paste_trigger: v })}
              />
            </Row>
            <Row label="支持纯文本粘贴" hint="面板内 Shift+Enter（或右键「仅粘贴文本」）粘贴无格式内容">
              <Toggle value={s.clip_paste_plain} onChange={(v) => patch({ clip_paste_plain: v })} />
            </Row>
            <Row label="过期天数" hint="超过 N 天的条目自动清理；0 = 永不过期">
              <input
                className="set-input"
                type="number"
                min={0}
                max={3650}
                step={1}
                value={s.clip_expire_days}
                onChange={(e) =>
                  patch({ clip_expire_days: Math.min(3650, Math.max(0, Number(e.target.value) || 0)) })
                }
              />
            </Row>
            <Row label="点击面板外自动关闭" hint="关掉则需 Esc/✕/热键才收起">
              <Toggle value={s.clip_hide_on_blur} onChange={(v) => patch({ clip_hide_on_blur: v })} />
            </Row>
            <Row label="列表密度" hint="紧凑 / 舒适 两档行高">
              <Select<"compact" | "comfortable">
                value={s.clip_list_density}
                choices={["compact", "comfortable"] as const}
                labels={{ compact: "紧凑", comfortable: "舒适" }}
                onChange={(v) => patch({ clip_list_density: v })} />
            </Row>
            <Row label="导出历史为 JSON" hint="导出 clip-history.json（图片另存到指定目录）">
              <button className="set-button" onClick={exportClipHistory}>导出…</button>
            </Row>
          </>
        )}

        {active === "save" && (
          <>
            <h1>保存</h1>
            <Row label="保存目录" hint="留空 = 系统图片目录下的 SnapPin">
              <div className="set-input-row">
                <input
                  className="set-input"
                  type="text"
                  placeholder="（系统图片目录 / SnapPin）"
                  value={s.save_dir}
                  onChange={(e) => patch({ save_dir: e.target.value })}
                />
                <button
                  className="set-button"
                  type="button"
                  onClick={browseSaveDir}
                >
                  浏览…
                </button>
              </div>
            </Row>
            <Row label="文件名格式">
              <Select
                value={s.save_name_format}
                choices={["timestamp", "sequence"] as const}
                labels={{ timestamp: "时间戳", sequence: "序号递增" }}
                onChange={(v) => patch({ save_name_format: v })}
              />
            </Row>
            <Row
              label="保存为 PNG"
              hint="开=PNG（无损）；关=JPG（有损、体积小）。截图与长截图按此存盘，扩展名随内容变"
            >
              <Toggle
                value={s.save_format_png}
                onChange={(v) => patch({ save_format_png: v })}
              />
            </Row>
            <Row
              label={`JPEG 画质（${s.jpeg_quality}）`}
              hint="存 JPG 时生效（40-100）：越高越清晰、体积越大；PNG 不受影响"
            >
              <input
                type="range"
                min={40}
                max={100}
                step={1}
                value={s.jpeg_quality}
                onChange={(e) =>
                  patch({
                    jpeg_quality: Math.min(100, Math.max(40, Number(e.target.value) || 92)),
                  })
                }
              />
            </Row>
          </>
        )}

        {active === "annotate" && (
          <>
            <h1>标注</h1>
            <Row label="默认画笔颜色" hint="打开截图工具栏时的初始颜色">
              <div className="palette">
                {PALETTE.map((c) => (
                  <button
                    key={c}
                    className={s.annotate_color.toLowerCase() === c ? "swatch active" : "swatch"}
                    style={{ background: c }}
                    onClick={() => patch({ annotate_color: c })}
                    aria-label={c}
                  />
                ))}
              </div>
            </Row>
            <Row label={`默认粗细（${s.annotate_stroke_width}px）`}>
              <input
                type="range"
                min={1}
                max={20}
                value={s.annotate_stroke_width}
                onChange={(e) => patch({ annotate_stroke_width: Number(e.target.value) })}
              />
            </Row>
          </>
        )}

        {active === "hotkeys" && (
          <>
            <Row label="禁用快捷键" hint="开启后不注册任何全局热键">
              <Toggle
                value={s.hotkeys_disabled}
                onChange={(v) => patch({ hotkeys_disabled: v })}
              />
            </Row>
            <Row
              label="全屏程序时自动禁用快捷键"
              hint="前台为 D3D 全屏/无边框全屏时临时注销热键，退出全屏自动恢复"
            >
              <Toggle
                value={s.hotkeys_disable_fullscreen}
                onChange={(v) => patch({ hotkeys_disable_fullscreen: v })}
              />
            </Row>
            <HotkeyEditor hotkeys={s.hotkeys} onSave={saveHotkey} />
          </>
        )}

        {active === "builtin" && (
          <>
            <h1>内置快捷键</h1>
            <p className="muted small">
              截图覆盖层（overlay）内的即时按键，同 PixPin「内置快捷键」页口径。
            </p>
            <div className="hk-list">
              {[
                ["关闭选区/退出截图", "Esc"],
                ["完成并保存复制", "Enter"],
                ["检测文本并复制", "Shift + C"],
                ["撤销标注", "Ctrl + Z"],
                ["重做标注", "Ctrl + Y"],
                ["清空全部标注", "Ctrl + Shift + Z"],
                ["切换元素/窗口识别", "Tab"],
                ["切换识别层级", "鼠标滚轮"],
                ["长截图", "L"],
                ["录制视频", "G"],
                ["贴图", "Ctrl + T"],
              ].map(([name, key]) => (
                <div className="hk-row" key={name}>
                  <span className="hk-name">{name}</span>
                  <span className="hk-key">{key}</span>
                </div>
              ))}
            </div>
          </>
        )}

        {active === "record" && (
          <>
            <h1>录制</h1>
            <Row label="帧率" hint="NVENC 硬编码，帧率越高越吃 GPU">
              <Select
                value={s.record_fps}
                choices={FPS_CHOICES}
                onChange={(v) => patch({ record_fps: v })}
              />
            </Row>
            <Row
              label="录制画质"
              hint="三档固定：实际体积随画面变化，档位越高细节越多（码率是档位键，不是承诺码率）"
            >
              <Select
                value={presetFor(s.record_bitrate_k)}
                choices={QUALITY_PRESETS.map((q) => q.k)}
                labels={Object.fromEntries(
                  QUALITY_PRESETS.map((q) => [String(q.k), `${q.label} · ${q.desc}`]),
                )}
                onChange={(v) => patch({ record_bitrate_k: v })}
              />
            </Row>
            <Row label={`开始倒计时（${s.record_countdown} 秒）`} hint="0 = 立即开录；按钮与录制热键都按它倒计时">
              <input
                type="range"
                min={0}
                max={10}
                value={s.record_countdown}
                onChange={(e) => patch({ record_countdown: Number(e.target.value) })}
              />
            </Row>
            <Row
              label="录音频"
              hint="区域/全屏录制走 dshow 设备（下面选）；窗口录制抓系统声音；GIF 模式不录音"
            >
              <Toggle value={s.record_audio} onChange={(v) => patch({ record_audio: v })} />
            </Row>
            {s.record_audio && (
              <Row
                label="音频设备"
                hint={
                  audioDevices.length
                    ? "「自动」= 取第一个可用设备"
                    : "本机未检测到音频输入设备（开启也录不到声音）"
                }
              >
                <Select
                  value={s.record_audio_device}
                  choices={[
                    ...new Set([
                      "",
                      ...audioDevices,
                      ...(s.record_audio_device ? [s.record_audio_device] : []),
                    ]),
                  ]}
                  labels={{ "": "自动（第一个可用设备）" }}
                  onChange={(v) => patch({ record_audio_device: v })}
                />
              </Row>
            )}
          </>
        )}

        {active === "ocr" && (
          <>
            <h1>OCR</h1>
            <Row label="识别后自动复制" hint="识别出的文字自动进剪贴板">
              <Toggle value={s.ocr_auto_copy} onChange={(v) => patch({ ocr_auto_copy: v })} />
            </Row>
            <p className="muted small">
              引擎：PP-OCRv5（本地离线，oar-ocr 原生推理），无需联网与密钥。
            </p>
          </>
        )}

        {active === "about" && (
          <>
            <h1>关于</h1>
            <div className="about">
              <p className="muted">
                SnapPin —— 截图 / 标注 / 贴图 / 长截图 / OCR / 录屏 一体工具。
              </p>
              <div className="kv">
                <span>Agent 接入</span>
                <span>{httpOk}</span>
              </div>
              {/* 如实列出**当前生效**的全局热键（用户覆盖优先，空则内置默认），并指向能改它的页面 */}
              <p className="muted small">
                全局快捷键（可在「快捷键/动作」页重绑）：
                {HOTKEY_ACTIONS.map((a) => `${fmtKey(s.hotkeys[a.id] || a.def)} ${a.name}`).join(" · ")}
              </p>
            </div>
          </>
        )}
      </main>
    </div>
  );
}

export default App;
