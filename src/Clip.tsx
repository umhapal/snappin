import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { DEFAULTS } from "./settings";
import type { Settings } from "./settings";
import { Icon } from "./icons";
// 面板颜色全部来自 theme.css 的 token，必须先引入 token 表再引组件样式
// （剪贴板窗是独立 webview，不会继承其它窗口的样式，漏引就是「满屏透明」）。
import "./theme.css";
import "./Clip.css";
/* ============================================================
 * SnapPin 剪贴板历史面板（Clip）—— 由 main.tsx 按 label "clip" 分流。
 * 挂载后拉取历史列表；点击条目 = 写回系统剪贴板（auto_paste 开启则直接粘进上个窗口）；
 * 每条 ★ 固定/取消固定、✕ 删除；底部「清空」；Esc 或标题栏 ✕ 隐藏窗口（不销毁）。
 * P1（2026-09-24）：搜索框（Ctrl+F + 200ms IME-safe 防抖）、固定置顶、触发方式
 * 单击/双击、Shift+Enter 纯文本粘贴、点外自动关、底栏计数、密度两档。
 * 读设置走 `get_settings`（与 Pin.tsx 同款），行为开关全在 Settings.clip_* 里。
 * ============================================================ */

type ClipEntry =
  | { kind: "text"; id: number; ts: number; text: string; pinned: boolean }
  | {
      kind: "image";
      id: number;
      ts: number;
      file: string;
      w: number;
      h: number;
      thumb?: string;
      // 动图条目（gif/webp/apng）：thumb 只是首帧，动图本体走 clip_animated 懒加载。
      animated?: boolean;
      animated_id?: string;
      pinned: boolean;
    }
  | {
      kind: "files";
      id: number;
      ts: number;
      paths: string[];
      // 动图文件（gif/webp/apng）：动图本体走 clip_animated 懒加载（D9）。
      animated?: boolean;
      animated_id?: string;
      pinned: boolean;
    };

function fmtTime(ts: number): string {
  const d = new Date(ts);
  const now = new Date();
  const hm = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  return d.toDateString() === now.toDateString() ? hm : `${d.getMonth() + 1}/${d.getDate()} ${hm}`;
}

/** 纯函数过滤：文本比内容、文件比路径、图片比「图片」类型词与尺寸串（前后端两处共用语义）。 */
function applyFilter(list: ClipEntry[], q: string): ClipEntry[] {
  const s = q.trim().toLowerCase();
  if (!s) return list;
  return list.filter((it) => {
    if (it.kind === "text") return it.text.toLowerCase().includes(s);
    if (it.kind === "files") return it.paths.join("\n").toLowerCase().includes(s);
    return "图片".includes(s) || `${it.w}×${it.h}`.toLowerCase().includes(s);
  });
}

// 动图条目的缩略图：clip_list 只带首帧 PNG，动图本体（多帧）按需 invoke("clip_animated")
// 拿 data URL 渲染（D9/C-3）。不把动图 base64 塞进 clip_list —— 面板条目多时每轮刷新
// 都要搬 MB 级数据，IPC 直接被拖死。首帧先垫上，动图加载完再替换，列表不会闪白。
function ClipThumb(props: { entry: { animated?: boolean; animated_id?: string; thumb?: string } }) {
  const it = props.entry;
  const [animatedSrc, setAnimatedSrc] = useState<string | null>(null);
  useEffect(() => {
    if (!it.animated || !it.animated_id) return;
    let dead = false;
    invoke<string | null>("clip_animated", { id: String(it.animated_id) })
      .then((dataUrl) => {
        if (!dead && dataUrl) setAnimatedSrc(dataUrl);
      })
      .catch((e) => console.error("加载动图失败", e));
    return () => {
      dead = true;
    };
  }, [it.animated, it.animated_id]);
  const src = animatedSrc ?? it.thumb ?? "";
  // 动图还没加载出来时不渲染空 <img>（会占位成一个破图）。
  if (!src) return null;
  return (
    <img
      className="clip-thumb"
      src={src}
      alt="剪贴板图片"
      loading="lazy"
      decoding="async"
    />
  );
}

function Clip() {
  const [items, setItems] = useState<ClipEntry[]>([]);
  const [copiedId, setCopiedId] = useState<number | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const [query, setQuery] = useState("");
  const [dq, setDq] = useState(""); // 200ms 防抖后的实际过滤词（IME 组合期间不动）
  const [activeId, setActiveId] = useState<number | null>(null);
  const [settings, setSettings] = useState<Settings>({ ...DEFAULTS });
  const settingsRef = useRef<Settings>({ ...DEFAULTS });
  const inputRef = useRef<HTMLInputElement>(null);
  const composingRef = useRef(false);
  // 拖动面板期间置真：拖动会触发 window blur，豁免「点面板外自动关闭」误判（#10）。
  const draggingRef = useRef(false);
  // 粘贴期间豁免「失焦自动关闭」：clip_paste 会把焦点还给目标窗，webview 随即 blur，
  // 若不放行则 clip_close_after_paste=false（保留面板供连续粘贴）会被失焦关闭立刻推翻。
  const pastingRef = useRef(false);
  // 面板是否已拿到键盘焦点（后端 clip_activate_panel 成功后置真；面板隐藏时复位）。
  const activatedRef = useRef(false);
  // 后端没有 clip_activate_panel（老版本）时置真，不再反复试错。
  const noActivateRef = useRef(false);

  // 让面板真正能收键盘：后端建窗起带 WS_EX_NOACTIVATE（面板整体不激活），
  // 所以「点搜索框 / Ctrl+F / 首次点条目」时必须先让后端撤掉 NOACTIVATE 并把焦点给
  // WebView2 子窗口，否则按键根本进不来（D13）。只在**用户主动交互**后调用 ——
  // 打开面板时就调等于又把用户原输入框的插入符抢走，正是 D13 要修的东西。
  const activatePanel = useCallback(() => {
    if (activatedRef.current || noActivateRef.current) return;
    activatedRef.current = true;
    void invoke("clip_activate_panel")
      .then(() => {
        // 面板激活后按开关把光标放进搜索框：用户已经主动点了面板，这时移动光标
        // 不会抢别的程序的插入符（打开面板那一刻就抢，那才是 D13 的病根）。
        if (settingsRef.current.clip_search_focus) inputRef.current?.focus();
      })
      .catch((e) => {
        activatedRef.current = false;
        const msg = String(e);
        if (/not found|not_found|未知|未找到/i.test(msg)) noActivateRef.current = true;
        console.warn("clip_activate_panel 失败：面板可能收不到键盘", e);
      });
  }, []);

  const reload = useCallback(() => {
    invoke<ClipEntry[]>("clip_list")
      .then(setItems)
      .catch(() => setItems([]));
  }, []);

  // 拉（或重拉）用户设置：挂载一次 + 每次面板显示再拉一次（设置页可能刚改过）。
  const loadSettings = useCallback(async () => {
    try {
      const s = await invoke<Settings>("get_settings");
      const merged = { ...DEFAULTS, ...s };
      setSettings(merged);
      settingsRef.current = merged;
      return merged;
    } catch (e) {
      console.error("get_settings 失败，用默认设置", e);
      return settingsRef.current;
    }
  }, []);

  const close = useCallback(() => {
    // 走后端 toggle_clip_history（而非直接 window.hide()）：面板现在会激活，关闭时若前台仍是
    // 本窗，由后端把焦点还给打开面板前的原输入程序（2026-10-14 方案 a）；前端自己 hide 会让
    // 后端拿不到关闭时机，原输入框就一直失焦。
    void (async () => {
      // 已隐藏就别再 toggle —— 那是切换语义，会把面板反向弹回来。用权威的窗口可见性判断，
      // 不依赖 Tauri hide 下不保证触发的 visibilitychange（2026-09-24 check-14 实测）。
      if (!(await getCurrentWindow().isVisible())) return;
      // 隐藏前按开关清空搜索词（#4）：webview 只 hide 不销毁，下次显示即干净列表。
      if (settingsRef.current.clip_search_clear_on_hide) {
        setQuery("");
        setDq("");
      }
      // 失败必须留痕（2026-09-23：void 吞错让 ✕/Esc 静默失效一整轮）。
      await invoke("toggle_clip_history");
    })().catch((e) => console.error("隐藏面板失败", e));
  }, []);

  // 挂载：拉设置。**这里不聚焦搜索框** —— 打开面板就抢焦点会把用户原输入框的插入符带走
  // （D13）。搜索框改为「用户主动点了面板 / 按 Ctrl+F」才聚焦。
  useEffect(() => {
    void loadSettings();
  }, [loadSettings]);

  // 面板显示/隐藏生命周期（webview hide 不销毁，事件监听拿不到回调 → 用 visibilitychange）：
  // 显示 = 重拉设置与历史（不抢焦点，D13）；隐藏 = 按开关清空搜索词（#4）+ 复位激活标记。
  useEffect(() => {
    const onVis = () => {
      if (document.visibilityState === "visible") {
        void loadSettings().then(() => {
          reload();
        });
      } else {
        // 面板被藏起来时后端会把它重新设回 NOACTIVATE，下次显示要重新激活。
        activatedRef.current = false;
        if (settingsRef.current.clip_search_clear_on_hide) {
          setQuery("");
          setDq("");
        }
      }
    };
    document.addEventListener("visibilitychange", onVis);
    return () => document.removeEventListener("visibilitychange", onVis);
  }, [loadSettings, reload]);

  // 面板显示事件（后端 show 后发 "clip-shown"）：Tauri hide/show 下 visibilitychange 不可靠，
  // 这是「重开面板重拉设置」的权威来源（2026-09-24 check-14 实测：缺它则密度/失焦开关改了不生效）。
  useEffect(() => {
    const un = listen("clip-shown", () => {
      void loadSettings().then(() => {
        reload();
      });
    });
    return () => {
      void un.then((f) => f());
    };
  }, [loadSettings, reload]);

  // 挂载即拉一次；此后靠事件驱动刷新（clip-shown / clip-changed），不再后台轮询——
  // 隐藏态每 1.5s 重拉全部缩略图曾把单进程 CPU/磁盘打满、拖慢所有窗口（卡顿根因）。
  useEffect(() => {
    reload();
  }, [reload]);

  // 后端捕获到新内容时发 clip-changed：仅面板可见才重拉（隐藏态不做无谓 IPC）。
  useEffect(() => {
    const un = listen("clip-changed", () => {
      if (document.visibilityState === "visible") reload();
    });
    return () => {
      void un.then((f) => f());
    };
  }, [reload]);

  // 点面板外自动关闭（#10）：面板内部点击/拖动会让 webview 瞬时 blur，只有焦点真正
  // 离开本窗（切到别的程序 / 自动粘贴把焦点还回上个窗口）才关。用宏任务延后 +
  // document.hasFocus() 复核过滤窗内焦点抖动，并在拖动期间豁免（用户实测「点窗内也被关掉」根因）。
  useEffect(() => {
    const onBlur = () => {
      if (!settingsRef.current.clip_hide_on_blur) return;
      if (draggingRef.current) return;
      if (pastingRef.current) return;
      setTimeout(() => {
        if (
          settingsRef.current.clip_hide_on_blur &&
          !draggingRef.current &&
          !pastingRef.current &&
          !document.hasFocus()
        ) {
          close();
        }
      }, 0);
    };
    window.addEventListener("blur", onBlur);
    return () => window.removeEventListener("blur", onBlur);
  }, [close]);

  // 搜索词 200ms 防抖（#搜索框本体）：IME 组合（拼音候选）期间不推进过滤词，
  // 组合结束立刻冲刷——避免打拼音中途列表乱跳、也避免候选词被当英文过滤。
  useEffect(() => {
    if (composingRef.current) return;
    const t = setTimeout(() => setDq(query), 200);
    return () => clearTimeout(t);
  }, [query]);

  const filtered = useMemo(() => applyFilter(items, dq), [items, dq]);

  const flashCopied = useCallback((id: number) => {
    setCopiedId(id);
    setTimeout(() => setCopiedId((c) => (c === id ? null : c)), 1200);
  }, []);

  // 写回/粘贴失败的用户可见提示：原来只 console.error，用户点了没反应也不知道
  // 成没成（D8 的「复制后要给反馈」在失败侧同样成立）。
  const [opError, setOpError] = useState<string | null>(null);
  const opErrorTimerRef = useRef<number | null>(null);
  const showOpError = useCallback((msg: string) => {
    setOpError(msg);
    if (opErrorTimerRef.current !== null) window.clearTimeout(opErrorTimerRef.current);
    opErrorTimerRef.current = window.setTimeout(() => setOpError(null), 4000);
  }, []);

  const copyBack = useCallback(
    (id: number) => {
      invoke("clip_copy_back", { id })
        .then(() => {
          flashCopied(id);
          reload();
        })
        .catch((e) => {
          console.error("写回失败", e);
          showOpError(`写回剪贴板失败：${typeof e === "string" ? e : String(e)}`);
        });
    },
    [flashCopied, reload, showOpError],
  );

  // 自动粘贴（#5）：写回 + 焦点还给上个窗口 + Ctrl+V，一条命令原子完成。
  // 仅在 clip_auto_paste 开启时走这里；关闭时点击只 copy_back（用户手动 Ctrl+V）。
  const doPaste = useCallback(
    (id: number) => {
      // 标记粘贴窗口：clip_paste 会把焦点还给目标窗触发 blur，期间豁免失焦自动关闭，
      // 让 clip_close_after_paste=false 真能保留面板（见 onBlur）。
      pastingRef.current = true;
      invoke("clip_paste", { id })
        .then(() => {
          flashCopied(id);
          reload();
        })
        .catch((e) => {
          console.error("粘贴失败", e);
          showOpError(`粘贴失败：${typeof e === "string" ? e : String(e)}`);
        })
        .finally(() => {
          setTimeout(() => (pastingRef.current = false), 400);
        });
    },
    [flashCopied, reload, showOpError],
  );

  // 标题栏空白处按住拖动面板（2026-09-24 用户点名「剪贴板能不能拖动」）。
  // 无边框窗用 core 的 startDragging；按钮（清空/✕）要留着点，不参与拖。
  // 位置不需要写回配置：窗 hide 复用、只建一次，位置天然跟着窗走。
  const onHeaderDrag = useCallback((e: React.MouseEvent) => {
    if (e.button !== 0) return;
    if ((e.target as HTMLElement).closest("button")) return;
    // 拖动全程屏蔽 blur 自动关闭：startDragging 本机瞬返，松手后窗口重获焦点再解除。
    draggingRef.current = true;
    void getCurrentWindow()
      .startDragging()
      .catch((err) => console.error("拖动面板失败", err))
      .finally(() => {
        setTimeout(() => (draggingRef.current = false), 300);
      });
  }, []);

  const togglePin = useCallback(
    (id: number) => {
      invoke("clip_toggle_pin", { id })
        .then(reload)
        .catch((e) => console.error("固定失败", e));
    },
    [reload],
  );

  const removeOne = useCallback(
    (id: number) => {
      invoke("clip_remove", { id })
        .then(reload)
        .catch((e) => console.error("删除失败", e));
    },
    [reload],
  );

  // 清空需二次确认（ClipRefResearch P0：一键清空全部历史无挽回是真实风险）：
  // 第一次点击只把按钮变「确认清空？」，3 秒内再点才真清。
  const clearAll = useCallback(() => {
    if (!confirmClear) {
      setConfirmClear(true);
      return;
    }
    setConfirmClear(false);
    invoke("clip_clear")
      .then(reload)
      .catch((e) => console.error("清空失败", e));
  }, [confirmClear, reload]);

  useEffect(() => {
    if (!confirmClear) return;
    const t = setTimeout(() => setConfirmClear(false), 3000);
    return () => clearTimeout(t);
  }, [confirmClear]);

  // 条目点击语义（#5/#6）：
  // - auto_paste 关：单击 = 写回剪贴板（旧行为），双击不再重复触发（detail 守卫）。
  // - auto_paste 开 + 单击档：单击直接粘。
  // - auto_paste 开 + 双击档：单击只高亮选中，双击才粘（Ditto/EcoPaste 同款）。
  const onItemClick = useCallback(
    (e: React.MouseEvent, id: number) => {
      if (e.detail > 1) return; // 双击的第一二跳 click，交给 onDoubleClick
      // 点了面板就说明用户要用键盘了：先让面板拿到焦点，↑↓/Enter 才收得到。
      activatePanel();
      const st = settingsRef.current;
      if (!st.clip_auto_paste) {
        copyBack(id);
        return;
      }
      setActiveId(id);
      if (st.clip_paste_trigger === "single") doPaste(id);
    },
    [activatePanel, copyBack, doPaste],
  );

  const onItemDoubleClick = useCallback(
    (_e: React.MouseEvent, id: number) => {
      if (settingsRef.current.clip_auto_paste) doPaste(id);
    },
    [doPaste],
  );

  // 键盘游标：在当前过滤结果（filtered）里移动 activeId 并滚动到可视区。
  // 无选中时 ↓ 取首条、↑ 取末条；到端点环绕（复制管理器惯例，避免卡死在边界）。
  const moveActive = useCallback(
    (dir: 1 | -1) => {
      if (filtered.length === 0) return;
      const cur = activeId == null ? -1 : filtered.findIndex((it) => it.id === activeId);
      const next =
        cur === -1
          ? dir === 1
            ? 0
            : filtered.length - 1
          : (cur + dir + filtered.length) % filtered.length;
      const id = filtered[next].id;
      setActiveId(id);
      // block:"nearest" 只滚必要距离，不打断用户手动滚动留下的位置记忆。
      document.querySelector(`.clip-item[data-id="${id}"]`)?.scrollIntoView({ block: "nearest" });
    },
    [activeId, filtered],
  );

  // 全局键盘：Esc 关闭（原行为）/ Ctrl+F 聚焦搜索 / ↑↓ 移动选中 / Enter 粘贴当前项
  // （无选中取过滤结果首条，auto_paste 决定真粘贴还是仅写回）/ Shift+Enter 纯文本粘贴（#7）。
  // IME 组合中（e.isComposing）一律不管，防拼音确认键被误吞。
  const onKey = useCallback(
    (e: KeyboardEvent) => {
      if (e.isComposing) return;
      if (e.key === "Escape") {
        e.preventDefault();
        close();
        return;
      }
      // 面板默认不激活（WS_EX_NOACTIVATE）：用户按 ↑↓/Enter 前先让它拿到键盘焦点，
      // 否则按键进不来（后端 clip_activate_panel）。
      activatePanel();
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "f") {
        e.preventDefault();
        // 面板默认不激活（WS_EX_NOACTIVATE），先让它拿到键盘焦点，否则按键根本进不来。
        activatePanel();
        inputRef.current?.focus();
        inputRef.current?.select();
        return;
      }
      if (e.key === "ArrowDown") {
        e.preventDefault(); // 防面板自身滚动：位移只由 scrollIntoView 负责
        moveActive(1);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        moveActive(-1);
        return;
      }
      // Shift+Enter：纯文本粘贴（#7），独立于下面的普通 Enter。
      if (e.key === "Enter" && e.shiftKey) {
        const st = settingsRef.current;
        if (!st.clip_paste_plain) return;
        // 目标 = 键盘聚焦的条目，没有则当前过滤结果第一条。
        const raw = (document.activeElement as HTMLElement | null)?.getAttribute("data-id");
        const id = raw != null && raw !== "" ? Number(raw) : (filtered[0]?.id ?? null);
        if (id == null || Number.isNaN(id)) return;
        e.preventDefault();
        if (st.clip_auto_paste) doPaste(id);
        else copyBack(id);
        return;
      }
      // Enter：粘贴当前选中项，无选中取过滤结果首条（与 ↑↓ 共用同一游标）。
      if (e.key === "Enter") {
        const st = settingsRef.current;
        const id = activeId ?? filtered[0]?.id ?? null;
        if (id == null) return;
        e.preventDefault();
        if (st.clip_auto_paste) doPaste(id);
        else copyBack(id);
      }
    },
    [close, copyBack, doPaste, activeId, filtered, moveActive, activatePanel],
  );

  useEffect(() => {
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onKey]);

  const dense = settings.clip_list_density === "compact";

  return (
    <div className="clip-root">
      {/* 标题栏：整条可拖动（onHeaderDrag 内部已排除 button，按钮不误触拖动） */}
      <div className="clip-header" onMouseDown={onHeaderDrag}>
        <strong className="clip-title">
          <Icon name="clipboard" />
          剪贴板历史
        </strong>
        <button
          className={`clip-hbtn danger${confirmClear ? " confirming" : ""}`}
          onClick={clearAll}
          title="清空全部历史（需点两次确认）"
        >
          {confirmClear ? "确认清空？" : "清空"}
        </button>
        <button className="clip-hbtn" onClick={close} title="关闭 (Esc)" aria-label="关闭">
          <Icon name="close" />
        </button>
      </div>

      {/* 操作失败提示：写回/粘贴失败原来只 console.error，界面上毫无反馈 */}
      {opError && (
        <div className="clip-flash" style={{ color: "#d6372e", padding: "2px 10px" }} role="status">
          <Icon name="close" />
          {opError}
        </div>
      )}

      {/* 搜索框（Ctrl+F 聚焦；IME-safe：composition 期间防抖过滤暂停） */}
      <div className="clip-search">
        <Icon name="search" />
        <input
          ref={inputRef}
          value={query}
          placeholder="搜索历史（Ctrl+F）"
          aria-label="搜索剪贴板历史"
          onChange={(e) => setQuery(e.target.value)}
          onFocus={() => activatePanel()}
          onCompositionStart={() => {
            composingRef.current = true;
          }}
          onCompositionEnd={() => {
            composingRef.current = false;
            setDq(query);
          }}
        />
      </div>

      <div className={`clip-list${dense ? " is-compact" : ""}`}>
        {items.length === 0 && (
          <div className="clip-empty">
            <Icon name="clipboard" />
            <div className="clip-empty-title">还没有剪贴板记录</div>
            <div className="clip-empty-hint">
              复制任意文字、图片或文件后，这里按时间倒序保留最近 {settings.clip_max} 条
            </div>
          </div>
        )}
        {items.length > 0 && filtered.length === 0 && (
          <div className="clip-empty">
            <Icon name="search" />
            <div className="clip-empty-title">没有匹配「{dq}」的记录</div>
            <div className="clip-empty-hint">换个更短的关键词，或按 Esc 关闭面板</div>
          </div>
        )}
        {filtered.map((it) => (
          <div
            key={it.id}
            data-id={it.id}
            tabIndex={0}
            role="button"
            className={`clip-item${it.pinned ? " is-pinned" : ""}${
              activeId === it.id ? " is-active" : ""
            }${copiedId === it.id ? " is-copied" : ""}`}
            onClick={(e) => onItemClick(e, it.id)}
            onDoubleClick={(e) => onItemDoubleClick(e, it.id)}
            onFocus={() => setActiveId(it.id)}
          >
            {/* 元信息行：左边只留「这是什么」（类型），右边是「什么时候 + 操作」。
                固定按钮移到右侧操作组——它和删除同类（都是对这条记录的操作），
                放最左会被误读成「这条的状态」。 */}
            <div className="clip-item-top">
              <span className="clip-type">
                <Icon name={it.kind === "text" ? "text" : it.kind === "image" ? "image" : "file"} />
                {it.kind === "text" ? "文本" : it.kind === "image" ? `图片 ${it.w}×${it.h}` : `文件 ${it.paths.length} 个`}
              </span>
              <span className="clip-spacer" />
              {copiedId === it.id && (
                <span className="clip-flash">
                  <Icon name="check" />
                  {settings.clip_auto_paste ? "已粘贴" : "已复制，Ctrl+V 粘贴"}
                </span>
              )}
              <span className="clip-time">{fmtTime(it.ts)}</span>
              {/* 固定（#9）：置顶显示、豁免超限删除与过期清理——后端 sort_entries 统一排序 */}
              <button
                className={`clip-ibtn${it.pinned ? " is-pinned" : ""}`}
                onClick={(e) => {
                  e.stopPropagation();
                  togglePin(it.id);
                }}
                title={it.pinned ? "取消固定" : "固定（置顶，不参与超限删除与过期清理）"}
                aria-label={it.pinned ? "取消固定" : "固定这条记录"}
              >
                <Icon name={it.pinned ? "star-filled" : "star"} />
              </button>
              <button
                className="clip-ibtn is-delete"
                onClick={(e) => {
                  e.stopPropagation();
                  removeOne(it.id);
                }}
                title="删除"
                aria-label="删除这条记录"
              >
                <Icon name="trash" />
              </button>
            </div>
            {/* 图片条目单独一支：缩略图不能套 3 行文字裁剪（-webkit-line-clamp 会把图切掉） */}
            {it.kind === "image" ? (
              <div className="clip-body is-image">
                <ClipThumb entry={it} />
              </div>
            ) : (
              <div className={`clip-body${it.kind === "files" ? " is-paths" : ""}`}>
                {/* 动图文件：上面放本体（多帧），下面照旧列路径（D9：动图不再只剩一张静图） */}
                {it.kind === "files" && it.animated ? <ClipThumb entry={it} /> : null}
                {it.kind === "text" ? it.text : it.paths.join("\n")}
              </div>
            )}
          </div>
        ))}
      </div>

      {/* 底栏（#12）：当前过滤下的条数 + 实际存在的快捷键提示（不写不存在的键） */}
      <div className="clip-foot">
        <span>
          共 {filtered.length} 条
          {dq && filtered.length !== items.length ? ` / ${items.length}` : ""}
        </span>
        {/* 只留最常用的两条 + 当前生效的粘贴方式：360px 宽放不下四条，
            堆满会挤成「一串没节奏的文本」（2026-09-24 评审）。
            Shift+Enter 纯文本属于低频进阶用法，设置页里有，底栏不再重复。 */}
        <span className="clip-foot-hints">
          <span>
            <kbd>Ctrl</kbd>+<kbd>F</kbd> 搜索
          </span>
          {settings.clip_auto_paste && (
            <span>{settings.clip_paste_trigger === "single" ? "单击" : "双击"}粘贴</span>
          )}
          <span>
            <kbd>Esc</kbd> 关闭
          </span>
        </span>
      </div>
    </div>
  );
}

export default Clip;
