import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type { CSSProperties, PointerEvent as RPointerEvent, WheelEvent as RWheelEvent } from "react";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { DEFAULTS, presetFor, QUALITY_PRESETS, type Settings } from "./settings";
import "./theme.css";
import "./Overlay.css";
import { Icon } from "./icons";

/* ============================================================
 * SnapPin 截图覆盖层（PixPin 式）—— docs/UI设计规范.md §1.5 / §3
 * 流程：抓整屏冻结帧 → 拖拽框选 → 选区下方浮标注工具条 → 标注 →
 *       合成「选区那块 + 标注」为最终 PNG → finish_capture → close_overlay
 * 坐标：DOM/交互一律用 CSS px；合成时按 scaleX/scaleY 映射回物理像素。
 * ============================================================ */

type Bounds = { x: number; y: number; w: number; h: number };
type Rect = { x: number; y: number; w: number; h: number };
type Pt = { x: number; y: number };
type WindowRect = { x: number; y: number; w: number; h: number; title: string };
// UIA 识别链上的一层（后端 ui_chain_at 的返回项，物理像素）。
type ElementInfo = { x: number; y: number; w: number; h: number; kind: string; name: string };

type Tool =
  | "rect"
  | "ellipse"
  | "arrow"
  | "pencil"
  | "text"
  | "number"
  | "mosaic"
  | "eyedropper";

// 有绘制样式（颜色/粗细）的墨迹工具，选中后弹二级 Popover
const INK_TOOLS: Tool[] = ["rect", "ellipse", "arrow", "pencil", "text", "number"];
// 「文字级」控件类型：常态悬停跳过它们（落到最近的按钮/编辑框等），
// 只有 Shift+C 检测文本时才用（见 resolveHover 与 copyTextAtCursor）。
const TEXT_KINDS = ["文本", "文档"];
// 马赛克按拖拽轨迹在标注 canvas 上像素化背景；取色器为即时取色（无 Shape）

type Shape =
  | { kind: "rect"; x: number; y: number; w: number; h: number; color: string; width: number }
  | { kind: "ellipse"; x: number; y: number; w: number; h: number; color: string; width: number }
  | { kind: "arrow"; x1: number; y1: number; x2: number; y2: number; color: string; width: number }
  | { kind: "pencil"; points: Pt[]; color: string; width: number }
  | { kind: "text"; x: number; y: number; text: string; color: string; size: number }
  | { kind: "number"; x: number; y: number; n: number; color: string; width: number }
  | { kind: "mosaic"; points: Pt[]; width: number };

type Handle = "nw" | "n" | "ne" | "e" | "se" | "s" | "sw" | "w";

type Gesture =
  | { mode: "new"; sx: number; sy: number }
  | { mode: "move"; sx: number; sy: number; orig: Rect }
  | { mode: "resize"; handle: Handle; sx: number; sy: number; orig: Rect }
  | { mode: "draw" }
  | null;

// 标注墨迹 6 色板（内容色，非 UI chrome，属于数据）
const PALETTE = ["#ef4444", "#f59e0b", "#eab308", "#22c55e", "#3b82f6", "#a855f7"];
const WIDTHS = [2, 4, 6];
const MIN_SEL = 6; // 小于此像素视为误触，取消选区
// 录制帧率档位：视频 15/24/30/60；GIF 限 10/15/24（GIF 高帧率意义不大且体积大）。
const REC_FPS_VIDEO = [15, 24, 30, 60];
const REC_FPS_GIF = [10, 15, 24];
// 画质三档定义在 settings.ts（与后端 QUALITY_PRESETS 同源），设置页与这里必须同口径。

// invoke 抛回来的错误多半是字符串；统一成一句能直接显示给用户的中文。
function errText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  try {
    return JSON.stringify(e);
  } catch {
    return String(e);
  }
}

// 暂存件路径形如 <产物目录>/.pending/SnapPin_xxx.png：命中就返回文件名，否则 null
// （录制产物的路径已是最终路径，不算暂存件）。
function pendingFileName(path: string): string | null {
  if (!/\.pending[\\/]/.test(path)) return null;
  return path.split(/[\\/]/).pop() ?? path;
}

// 录制计时格式化：秒 → mm:ss。
function fmtElapsed(sec: number): string {
  const m = Math.floor(sec / 60);
  const s = sec % 60;
  return `${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
}

function normRect(x1: number, y1: number, x2: number, y2: number): Rect {
  return {
    x: Math.min(x1, x2),
    y: Math.min(y1, y2),
    w: Math.abs(x2 - x1),
    h: Math.abs(y2 - y1),
  };
}

function normalize(r: Rect): Rect {
  let { x, y, w, h } = r;
  if (w < 0) {
    x += w;
    w = -w;
  }
  if (h < 0) {
    y += h;
    h = -h;
  }
  return { x, y, w, h };
}

function resizeRect(orig: Rect, handle: Handle, dx: number, dy: number): Rect {
  let { x, y, w, h } = orig;
  if (handle.includes("w")) {
    x += dx;
    w -= dx;
  }
  if (handle.includes("e")) w += dx;
  if (handle.includes("n")) {
    y += dy;
    h -= dy;
  }
  if (handle.includes("s")) h += dy;
  return normalize({ x, y, w, h });
}

function handlePoints(r: Rect): { id: Handle; cx: number; cy: number }[] {
  const { x, y, w, h } = r;
  return [
    { id: "nw", cx: x, cy: y },
    { id: "n", cx: x + w / 2, cy: y },
    { id: "ne", cx: x + w, cy: y },
    { id: "e", cx: x + w, cy: y + h / 2 },
    { id: "se", cx: x + w, cy: y + h },
    { id: "s", cx: x + w / 2, cy: y + h },
    { id: "sw", cx: x, cy: y + h },
    { id: "w", cx: x, cy: y + h / 2 },
  ];
}

function hitHandle(r: Rect, px: number, py: number): Handle | null {
  const R = 8;
  for (const p of handlePoints(r)) {
    if (Math.abs(px - p.cx) <= R && Math.abs(py - p.cy) <= R) return p.id;
  }
  return null;
}

// ---- 放大镜（rAF + ref 直绘版）----
// 不再走 React state：光标位置存 cursorRef，rAF 循环里比对上次绘制坐标，
// 变了才重绘 canvas 与文本，位置用 style 直改——每帧零 setState、零重渲染。
const LOUPE_VIEW = 128; // 放大镜显示边长（CSS px）
const LOUPE_ZOOM = 8; // 放大倍数
const LOUPE_SRC = Math.round(LOUPE_VIEW / LOUPE_ZOOM); // 取样的物理像素块边长

function drawLoupeFrame(
  ctx: CanvasRenderingContext2D,
  img: HTMLImageElement,
  scaleX: number,
  scaleY: number,
  cssX: number,
  cssY: number,
) {
  const VIEW = LOUPE_VIEW;
  const SRC = LOUPE_SRC;
  const px = Math.round(cssX * scaleX);
  const py = Math.round(cssY * scaleY);
  const half = Math.floor(SRC / 2);
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.imageSmoothingEnabled = false;
  ctx.clearRect(0, 0, VIEW, VIEW);
  ctx.fillStyle = "#111";
  ctx.fillRect(0, 0, VIEW, VIEW);
  try {
    // 从冻结背景取 SRC×SRC 物理像素块，最近邻放大铺满画布
    ctx.drawImage(img, px - half, py - half, SRC, SRC, 0, 0, VIEW, VIEW);
  } catch {
    /* 取样越界忽略 */
  }
  const cell = VIEW / SRC; // 单个源像素在放大镜里的边长
  // 淡网格线（像素栅格感）
  ctx.strokeStyle = "rgba(0,0,0,0.12)";
  ctx.lineWidth = 1;
  ctx.beginPath();
  for (let i = 0; i <= SRC; i++) {
    const p = Math.round(i * cell) + 0.5;
    ctx.moveTo(p, 0);
    ctx.lineTo(p, VIEW);
    ctx.moveTo(0, p);
    ctx.lineTo(VIEW, p);
  }
  ctx.stroke();
  // 中心取样像素双色描边方框（深底浅面，任意背景都可见）
  const c0 = Math.round(half * cell);
  ctx.strokeStyle = "rgba(0,0,0,0.9)";
  ctx.lineWidth = 1;
  ctx.strokeRect(c0 - 0.5, c0 - 0.5, cell + 1, cell + 1);
  ctx.strokeStyle = "#ffffff";
  ctx.lineWidth = 1;
  ctx.strokeRect(c0 + 0.5, c0 + 0.5, cell - 1, cell - 1);
}

// 放大镜盒子定位：光标右下方，贴近右/下边缘时翻转到反侧。
// 边界 = 光标所在显示器的物理矩形（不是视口：overlay 视口是整个虚拟桌面并集，
// 跨屏时按视口夹紧会把盒子推到物理屏外）。单屏/未抓帧时 `mon` 为空 → 退回视口。
function loupeBoxPos(cssX: number, cssY: number, mon?: Bounds | null): { left: number; top: number } {
  const OFF = 18;
  const BOX = LOUPE_VIEW + 2;
  const PANEL = 26;
  const mx = mon ? mon.x : 0;
  const my = mon ? mon.y : 0;
  const mw = mon ? mon.w : window.innerWidth;
  const mh = mon ? mon.h : window.innerHeight;
  let left = cssX + OFF;
  let top = cssY + OFF;
  if (left + BOX > mx + mw - 8) left = cssX - OFF - BOX;
  if (top + BOX + PANEL > my + mh - 8) top = cssY - OFF - BOX - PANEL;
  return { left, top };
}


// 录制条定位（A+ 2026-09-24）：**绝不能落进选区**（选区=crop，条子会被录进成品）。
// 优先级：上方放得下 → 下方放得下 → 横向左右逃逸（选区不满宽）→ 极端全屏选区
// 无处可逃时退回贴屏 clamp（承认入画；红框此时已在 crop 外，仅条子可见——边角登记）。
function recbarPos(
  sel: { x: number; y: number; w: number; h: number },
  vw: number,
  vh: number,
  // 选区所在显示器的物理矩形（CSS px）。缺省回退整个视口——overlay 视口是虚拟桌面
  // 并集，按它定位会把条子推到物理屏外（2026-10-15 用户报障）。
  mon?: { x: number; y: number; w: number; h: number } | null,
) {
  const BAR_W = 320; // 条宽上界（含暂停键），与旧 left clamp 同口径
  const BAR_H = 44;
  const GAP = 10;
  const EDGE = 8;
  const mx = mon ? mon.x : 0;
  const my = mon ? mon.y : 0;
  const mw = mon ? mon.w : vw;
  const mh = mon ? mon.h : vh;
  const centeredLeft = () =>
    Math.max(mx + EDGE, Math.min(sel.x + sel.w / 2 - 150, mx + mw - BAR_W));
  if (sel.y - BAR_H > my + EDGE) {
    return { top: sel.y - BAR_H, left: centeredLeft() };
  }
  if (sel.y + sel.h + GAP + BAR_H <= my + mh - EDGE) {
    return { top: sel.y + sel.h + GAP, left: centeredLeft() };
  }
  const midTop = Math.max(
    my + EDGE,
    Math.min(sel.y + sel.h / 2 - BAR_H / 2, my + mh - BAR_H - EDGE),
  );
  if (sel.x + sel.w + GAP + BAR_W <= mx + mw - EDGE) {
    return { top: midTop, left: sel.x + sel.w + GAP };
  }
  if (sel.x - BAR_W - GAP >= mx + EDGE) {
    return { top: midTop, left: sel.x - BAR_W - GAP };
  }
  return {
    top: Math.max(my + EDGE, Math.min(sel.y + sel.h + GAP, my + mh - 52)),
    left: centeredLeft(),
  };
}

function Overlay() {
  const [dataUrl, setDataUrl] = useState<string>("");
  const [ready, setReady] = useState(false); // 背景加载 + 尺寸算好
  const [selection, setSelection] = useState<Rect | null>(null);
  const [committed, setCommitted] = useState(false); // 选区已定，显示工具条
  const [tool, setTool] = useState<Tool | null>(null);
  const [color, setColor] = useState(PALETTE[0]);
  const [width, setWidth] = useState(WIDTHS[1]);
  // 用户设置（默认标注色/粗细、尺寸标签、UI 检测开关等）；拉不到用 DEFAULTS。
  const [settings, setSettings] = useState<Settings>(DEFAULTS);
  const settingsRef = useRef<Settings>(DEFAULTS);
  // 拉（或重拉）用户设置。设置窗口与 overlay 是同进程的两个 webview，overlay 页面只在
  // 首次截图时加载一次、之后一直隐藏复用：热键能被重绑、识别开关能被改，所以每次显示
  // overlay 都要重拉一次，否则左下角提示卡会拿旧热键糊弄用户。失败时沿用上一次的值。
  const loadSettings = useCallback(async () => {
    try {
      const s = await invoke<Settings>("get_settings");
      const merged = { ...DEFAULTS, ...s };
      setSettings(merged);
      settingsRef.current = merged;
      return merged;
    } catch (e) {
      console.error("get_settings 失败，用默认设置", e);
      return null;
    }
  }, []);
  useEffect(() => {
    void loadSettings().then((merged) => {
      if (!merged) return;
      setColor(merged.annotate_color);
      setWidth(merged.annotate_stroke_width);
      // 录制参数初值：帧率就近落到视频合法档，码率落回三档（不超过原值的那一档）。
      setRecFps(REC_FPS_VIDEO.includes(merged.record_fps) ? merged.record_fps : 30);
      // 旧配置里的自由码率落回三档（规则与后端 recorder::preset_for 同源）。
      setRecBitrate(presetFor(merged.record_bitrate_k));
      setRecAudio(merged.record_audio);
    });
  }, [loadSettings]);
  const [shapes, setShapes] = useState<Shape[]>([]);
  const [redo, setRedo] = useState<Shape[]>([]);
  const [draft, setDraft] = useState<Shape | null>(null);
  const [textEdit, setTextEdit] = useState<{ x: number; y: number } | null>(null);
  const [textValue, setTextValue] = useState("");
  const [busy, setBusy] = useState(false);
  // M2：窗口检测 / 取色 / OCR 结果浮层
  const [windows, setWindows] = useState<WindowRect[]>([]);
  const [ocrText, setOcrText] = useState<string | null>(null);
  // OCR 结果是否已进剪贴板（浮层标题据此说实话；复制动作由后端按 ocr_auto_copy 做）。
  const [ocrCopied, setOcrCopied] = useState(false);
  // 录制态：mode 区分视频/GIF（后端目前统一产 MP4），startedAt 供计时，pausedAt 非空=已暂停。
  // null=未录制。
  const [recording, setRecording] = useState<{
    mode: "video" | "gif";
    startedAt: number;
    pausedAt: number | null;
  } | null>(null);
  const [recElapsed, setRecElapsed] = useState(0); // 已录制秒数
  // GIF 转码进度（0-99）；null=未在转码。停止 GIF 录制后后端要跑 palettegen/paletteuse，
  // 长录可达几十秒，没有它用户只会看到「停止后卡住」。
  const [transcode, setTranscode] = useState<number | null>(null);
  // 保存确认条（capture_confirm_save 开启时）：产物已落默认路径，等用户
  // 快速保存 / 另存为… / 删除。null = 无待确认。
  const [savePrompt, setSavePrompt] = useState<{ kind: "png" | "jpg" | "mp4" | "gif"; path: string } | null>(
    null,
  );
  // 已发出停止、后端还在收尾（封段 → 拼接 → GIF 转码）的窗口期。这段时间 recording
  // 还不为 null，计时若照走就是用户看到的「按了停止还在录/还在渲染」（2026-10-14 报障）。
  // 转码进度事件另有 transcode 状态；这里只负责冻结计时与按钮。
  const [stopping, setStopping] = useState(false);
  // 结果条「复制」的短反馈（1.2s 后回文案）。
  const [copiedFlash, setCopiedFlash] = useState(false);
  // 操作反馈条：保存/复制的成功与失败提示。D12 的病根是「落盘失败只 console.error，
  // 界面停在原地既不弹条也不给提示」——用户以为按钮没点到，只能反复点。
  const [flash, setFlash] = useState<{ kind: "ok" | "err"; text: string } | null>(null);
  const flashTimerRef = useRef<number | null>(null);
  // 录音频开关（区域/全屏录制走 dshow 设备；后端 start_record 只读配置，不收参数）。
  const [recAudio, setRecAudio] = useState<boolean>(true);
  // 长截图路径专用：它先关 overlay 跑拼接、再重开 overlay 承载确认条，
  // 而 overlay-shown 的重置逻辑会清 savePrompt —— 这个标记让那一次重置放过它。
  const keepPromptRef = useRef(false);
  // 回放播放器（mp4）：重播按钮与空格切换共用；GIF 走 <img> 循环预览，无 player。
  const videoRef = useRef<HTMLVideoElement | null>(null);
  // 录制参数（开录前可调，进入录制后固定）：模式、帧率、码率。初值随设置。
  const [recMode, setRecMode] = useState<"video" | "gif">("video");
  const [recFps, setRecFps] = useState<number>(24);
  const [recBitrate, setRecBitrate] = useState<number>(15000);
  // 开录倒计时（settings.record_countdown > 0）：null=未倒计时；否则在选区上显示 secs 秒，
  // 归零后才 invoke start_record。mode 记下发起时的模式（G 键固定 video）。
  const [countdown, setCountdown] = useState<{ secs: number; mode: "video" | "gif" } | null>(null);
  // 取景/移动/缩放手势进行中显示三分辅助线，松手隐藏。
  const [gridOn, setGridOn] = useState(false);

  const imgRef = useRef<HTMLImageElement | null>(null);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const scaleRef = useRef({ x: 1, y: 1 }); // 物理/CSS
  const gestureRef = useRef<Gesture>(null);
  const numRef = useRef(1); // 序号计数
  const capturedOnce = useRef(false);

  const boundsRef = useRef<Bounds>({ x: 0, y: 0, w: 0, h: 0 });
  // 显示器物理矩形（抓帧时下发，见 capture_screen_data）。**不是**视口尺寸：
  // overlay 覆盖整个虚拟桌面并集，用视口夹紧会把浮层推出物理屏（2026-10-15 用户报障）。
  const monitorsRef = useRef<Bounds[]>([]);
  // 浮层定位基准：选区所在显示器的物理矩形（overlay CSS 坐标系）。查不到（未抓帧、
  // 单屏退化）返回 null，调用方回退整个视口。用选区**中心**判定归属：跨屏选区取中心所在那块。
  const uiMonitor = useCallback((sel: { x: number; y: number; w: number; h: number }): Bounds | null => {
    const list = monitorsRef.current;
    const b = boundsRef.current;
    if (!list.length || !b.w || !b.h) return null;
    // overlay 的 CSS 视口 = bounds 物理矩形按同一比例缩放（Per-Monitor-V2 下通常 1:1）。
    const s = { x: window.innerWidth / b.w, y: window.innerHeight / b.h };
    const cx = b.x + (sel.x + sel.w / 2) / s.x;
    const cy = b.y + (sel.y + sel.h / 2) / s.y;
    const hit = list.find((m) => cx >= m.x && cx <= m.x + m.w && cy >= m.y && cy <= m.y + m.h);
    if (!hit) return null;
    return { x: (hit.x - b.x) * s.x, y: (hit.y - b.y) * s.y, w: hit.w * s.x, h: hit.h * s.y };
  }, []);

  // 取色用：整屏背景像素采样画布（惰性构建）
  const bgSampleRef = useRef<{ ctx: CanvasRenderingContext2D } | null>(null);
  const hoverWinRef = useRef<Rect | null>(null); // 当前命中窗口（CSS px）
  const hoverLabelRef = useRef<string | null>(null); // 命中对象的可读标签（元素类型/层级/名称）
  const pendingWinRef = useRef<Rect | null>(null); // 按下时命中的窗口，供点击选窗
  // 选区镜像：异步路径（UIA 在途回调的微任务）必须在 React 提交前就能判断
  // 「选区已存在/手势进行中」，等 useEffect 同步会晚一拍——微任务先于渲染落地。
  const selectionRef = useRef<Rect | null>(null);
  selectionRef.current = selection; // 渲染期直写（幂等，StrictMode 安全）
  // UIA 智能识别：last=上次查询时间戳（节流），chain=光标处元素祖先链（内→外，物理像素），
  // inflight=是否有查询在途，key=链最内层元素的身份（变了才把层级归零），
  // level=当前查看的层级，mode=元素/窗口两种识别模式（Tab 切换）。
  const uiaLastRef = useRef(0);
  const uiaChainRef = useRef<ElementInfo[]>([]);
  const uiaInflightRef = useRef(false);
  const uiaKeyRef = useRef<string | null>(null);
  const uiaLevelRef = useRef(0);
  const recogModeRef = useRef<"element" | "window" | "text">("element");
  const recogElRef = useRef<HTMLDivElement | null>(null); // 识别标签（rAF 直改文本/位置）
  const lastPointerRef = useRef<{ x: number; y: number } | null>(null); // 最近一次光标的物理坐标

  // 智能识别的落点解析：元素模式取祖先链上第 level 层（useChain=false 时只认顶层
  // 窗口，用于「光标已离开上次命中的元素、新链还没回来」的过渡帧），窗口模式取光标
  // 下的顶层窗口。同时写下高亮矩形与可读标签（rAF 循环读这两个 ref 直绘）。
  // 定义位置必须早于下面 keydown 效果（它把本函数放进依赖数组，晚定义会 TDZ）。
  //
  // 稳定窗（2026-09-21 用户反馈「智能识别太灵敏，稍微晃一下就会切换窗口」）：
  // UIA 链一变就立刻换高亮，光标微抖/快速划过时高亮会连续闪跳，很难瞄准。
  // 改为：新候选先记进 hoverPendingRef，**连续稳定 HOVER_DWELL_MS 才提交**；
  // 期间候选又变了就重置计时（等于只有「停住」才会切换）。
  const HOVER_DWELL_MS = 140;
  const hoverKeyRef = useRef<string | null>(null);
  const hoverPendingRef = useRef<{
    rect: Rect | null;
    label: string | null;
    key: string;
    ts: number;
  } | null>(null);

  const resolveHover = useCallback(
    (gx: number, gy: number, useChain: boolean, immediate = false) => {
      // 智能识别只在「自由框选态」更新：手势进行中或选区已存在时，任何来源
      // （在途 UIA 回调晚到、稳定窗 dwell 提交）都不许再写高亮——否则识别框
      // 与选区同屏 = 用户 2026-09-24 实拍「套了两个选中框」。
      if (gestureRef.current || selectionRef.current) return;
      const { x: sx, y: sy } = scaleRef.current;
      const b = boundsRef.current;
      const toCss = (x: number, y: number, w: number, h: number): Rect => ({
        x: (x - b.x) / sx,
        y: (y - b.y) / sy,
        w: w / sx,
        h: h / sy,
      });
      let rect: Rect | null = null;
      let label: string | null = null;
      if (recogModeRef.current !== "window" && useChain) {
        const chain = uiaChainRef.current;
        if (chain.length) {
          // 常态（element）跳过「文本/文档」这类叶子：用户 2026-09-21 反馈
          // 「智能识图太宽泛（太细），常态不该报文字级」——悬停落在最近的
          // 「有意义控件」（按钮/编辑框/列表项…）上。滚轮上滚仍可继续钻到文字级；
          // 文字级识别走单独的 Shift+C（检测文本并复制）。
          const textMode = recogModeRef.current === "text";
          let base = 0;
          if (!textMode) {
            const i = chain.findIndex((e) => !TEXT_KINDS.includes(e.kind));
            base = i < 0 ? chain.length - 1 : i;
          }
          const lvl = Math.max(0, Math.min(base + uiaLevelRef.current, chain.length - 1));
          const e = chain[lvl];
          rect = toCss(e.x, e.y, e.w, e.h);
          const parts = [e.kind || "元素", `${lvl + 1}/${chain.length}`];
          if (e.name) parts.push(e.name);
          label = parts.join(" · ");
        }
      }
      if (!rect) {
        const win = windows.find(
          (w) => gx >= w.x && gx <= w.x + w.w && gy >= w.y && gy <= w.y + w.h,
        );
        rect = win ? toCss(win.x, win.y, win.w, win.h) : null;
        label = win ? `窗口 · ${win.title || "未命名"}` : null;
      }
      // 候选指纹：矩形取整 + 标签，避免亚像素抖动被当成换目标。
      const key = rect
        ? `${Math.round(rect.x)},${Math.round(rect.y)},${Math.round(rect.w)},${Math.round(
            rect.h,
          )}|${label ?? ""}`
        : "";
      if (key === hoverKeyRef.current) {
        hoverPendingRef.current = null;
        return;
      }
      if (immediate) {
        hoverKeyRef.current = key;
        hoverPendingRef.current = null;
        hoverWinRef.current = rect;
        hoverLabelRef.current = label;
        return;
      }
      const p = hoverPendingRef.current;
      if (!p || p.key !== key) {
        hoverPendingRef.current = { rect, label, key, ts: performance.now() };
      }
    },
    [windows],
  );

  // 挂载即拉取顶层窗口列表（物理像素，供悬停高亮 / 点击选窗）
  useEffect(() => {
    invoke<WindowRect[]>("list_windows")
      .then((ws) => setWindows(ws))
      .catch((e) => console.error("list_windows 失败", e));
  }, []);
  // 放大镜命令式 DOM：rAF 循环直绘，不走 state（卡顿修复，0.13 问题⑤）
  const loupeRef = useRef<HTMLDivElement | null>(null);
  const loupeCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const loupeVisibleRef = useRef(false); // 上帧是否可见（切换显示/隐藏时直改 style）
  const cursorRef = useRef<{ x: number; y: number; on: boolean }>({ x: 0, y: 0, on: false });

  // 1) 挂载即抓整屏冻结帧
  const hiElRef = useRef<HTMLDivElement | null>(null); // 窗口高亮 div（rAF 直改 style）
  // 页面就绪信号：Rust 侧看门狗据此判断 overlay 页面是否真的加载成功，
  // 没报到就自动关窗（overlay 是全屏置顶窗，留在屏幕上会吃掉整个桌面的点击）。
  useEffect(() => {
    void invoke("overlay_ready").catch(() => {});
  }, []);
  // ---- 抓冻结帧 → 亮窗（顺序不可颠倒）----
  // 2026-09-22 用户实测「截图页面里还有一张截图页面」的根因：后端 `open_overlay` 复用窗口时
  // 先 show 再 emit，页面异步重置 state 时，上一轮留在 DOM 里的冻结帧大图 + 工具条已经回到
  // 屏幕上，紧接着的 GDI BitBlt 把它们原样拍进新帧 → 递归截屏。
  // 现在后端只 emit、不 show；页面必须在**窗口不可见**时抓帧，抓到再调 `show_overlay` 亮窗。
  const grabFrame = useCallback(async () => {
    try {
      const d = await invoke<{ data_url: string; bounds: Bounds; epoch: number; monitors?: Bounds[] }>(
        "capture_screen_data",
      );
      boundsRef.current = d.bounds;
      monitorsRef.current = d.monitors ?? [];
      setDataUrl(d.data_url);
      // 带上**抓帧那一刻**的世代号：若这期间用户已经按 Esc/✕ 关了窗，后端会拒绝显示
      // （否则表现为「关掉又自己冒出来」）。
      await invoke("show_overlay", { epoch: d.epoch });
    } catch (e) {
      // 抓帧失败 → 覆盖层没有内容可显示，立刻自关，绝不留一个全屏空窗吃掉桌面输入。
      console.error("capture_screen_data 失败", e);
      void invoke("close_overlay");
    }
  }, []);
  useEffect(() => {
    if (capturedOnce.current) return; // StrictMode 双调用保护
    capturedOnce.current = true;
    void grabFrame();
  }, [grabFrame]);

  // 背景图加载完成：算 CSS→物理缩放，设定 canvas 物理尺寸
  const onBgLoad = useCallback(() => {
    const img = imgRef.current;
    const cvs = canvasRef.current;
    if (!img) return;
    const sx = img.naturalWidth / window.innerWidth;
    const sy = img.naturalHeight / window.innerHeight;
    scaleRef.current = { x: sx, y: sy };
    if (cvs) {
      cvs.width = img.naturalWidth;
      cvs.height = img.naturalHeight;
    }
    setReady(true);
  }, []);



  // 取某 CSS 坐标处背景图的像素颜色，返回 #RRGGBB；惰性把整屏背景画进采样画布
  const sampleColor = useCallback((cssX: number, cssY: number): string | null => {
    const img = imgRef.current;
    if (!img) return null;
    let sample = bgSampleRef.current;
    if (!sample) {
      const c = document.createElement("canvas");
      c.width = img.naturalWidth;
      c.height = img.naturalHeight;
      const cx = c.getContext("2d", { willReadFrequently: true });
      if (!cx) return null;
      cx.drawImage(img, 0, 0);
      sample = { ctx: cx };
      bgSampleRef.current = sample;
    }
    const { x: sx, y: sy } = scaleRef.current;
    const px = Math.max(0, Math.min(img.naturalWidth - 1, Math.round(cssX * sx)));
    const py = Math.max(0, Math.min(img.naturalHeight - 1, Math.round(cssY * sy)));
    const d = sample.ctx.getImageData(px, py, 1, 1).data;
    const hex = (n: number) => n.toString(16).padStart(2, "0");
    return `#${hex(d[0])}${hex(d[1])}${hex(d[2])}`;
  }, []);
  // ---- 放大镜 rAF 循环：只读 cursorRef，坐标没变不重绘；零 setState（0.13 问题⑤）----
  useEffect(() => {
    let raf = 0;
    let lastDrawn = { x: -1, y: -1, on: false };
    const tick = () => {
      raf = requestAnimationFrame(tick);
      const cur = cursorRef.current;
      const box = loupeRef.current;
      const img = imgRef.current;
      if (!cur.on || !box || !img || !ready) {
        if (loupeVisibleRef.current && box) {
          box.style.display = "none";
          loupeVisibleRef.current = false;
        }
        return;
      }
      if (cur.x === lastDrawn.x && cur.y === lastDrawn.y && loupeVisibleRef.current) return;
      lastDrawn = { x: cur.x, y: cur.y, on: true };
      const { x: sx, y: sy } = scaleRef.current;
      const c = loupeCanvasRef.current;
      const ctx = c?.getContext("2d");
      if (!ctx) return;
      drawLoupeFrame(ctx, img, sx, sy, cur.x, cur.y);
      // 信息条：物理坐标 + 中心像素 HEX（复用 sampleColor 的采样画布）
      const hex = sampleColor(cur.x, cur.y);
      const posEl = box.querySelector<HTMLElement>(".overlay-loupe-pos");
      const hexEl = box.querySelector<HTMLElement>(".overlay-loupe-hexval");
      const swEl = box.querySelector<HTMLElement>(".overlay-loupe-hex i");
      if (posEl) posEl.textContent = `${Math.round(cur.x * sx)}, ${Math.round(cur.y * sy)}`;
      if (hexEl) hexEl.textContent = hex ? hex.toUpperCase() : "—";
      if (swEl) swEl.style.background = hex ?? "transparent";
      // 盒子定位：右下优先，贴边翻转。边界按光标所在显示器（跨屏时视口夹紧会推出屏外）。
      const p = loupeBoxPos(cur.x, cur.y, uiMonitor({ x: cur.x, y: cur.y, w: 0, h: 0 }));
      box.style.left = `${p.left}px`;
      box.style.top = `${p.top}px`;
      box.style.display = "";
      loupeVisibleRef.current = true;
    };
    // 窗口高亮矩形也由 rAF 循环直改 style（hoverWinRef 由 move/UIA 回调更新）；
    // 识别标签（元素类型/层级/名称）同帧直改文本与位置，避免每帧 setState。
    // 注意：tick 必须在这里显式起播——0.14 改命令式 DOM 时漏了这行，
    // 放大镜/取色器从此永不显示（tick 只会在自己内部续帧，没人起第一帧）。
    raf = requestAnimationFrame(tick);
    const applyHi = () => {
      // 稳定窗提交：候选停住 HOVER_DWELL_MS 才真正切换高亮（见 resolveHover 注释）。
      const p = hoverPendingRef.current;
      if (p && performance.now() - p.ts >= HOVER_DWELL_MS) {
        hoverKeyRef.current = p.key;
        hoverWinRef.current = p.rect;
        hoverLabelRef.current = p.label;
        hoverPendingRef.current = null;
      }
      const hi = hoverWinRef.current;
      const hiEl = hiElRef.current;
      const recogEl = recogElRef.current;
      if (hiEl) {
        if (hi) {
          hiEl.style.left = `${hi.x}px`;
          hiEl.style.top = `${hi.y}px`;
          hiEl.style.width = `${hi.w}px`;
          hiEl.style.height = `${hi.h}px`;
          hiEl.style.display = "";
        } else {
          hiEl.style.display = "none";
        }
      }
      if (recogEl) {
        const label = hoverLabelRef.current;
        if (hi && label) {
          recogEl.textContent = label;
          recogEl.style.left = `${Math.max(2, hi.x)}px`;
          recogEl.style.top = `${Math.max(2, hi.y - 22)}px`;
          recogEl.style.display = "";
        } else {
          recogEl.style.display = "none";
        }
      }
    };
    const tick2 = () => {
      applyHi();
      raf2 = requestAnimationFrame(tick2);
    };
    let raf2 = requestAnimationFrame(tick2);
    return () => {
      cancelAnimationFrame(raf);
      cancelAnimationFrame(raf2);
    };
  }, [ready, sampleColor, uiMonitor]);

  // ---- 画布重绘：把已提交 shapes + 当前 draft 画到标注 canvas ----
  const drawShape = useCallback((ctx: CanvasRenderingContext2D, s: Shape) => {
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    switch (s.kind) {
      case "rect":
        ctx.strokeStyle = s.color;
        ctx.lineWidth = s.width;
        ctx.strokeRect(s.x, s.y, s.w, s.h);
        break;
      case "ellipse": {
        ctx.strokeStyle = s.color;
        ctx.lineWidth = s.width;
        ctx.beginPath();
        ctx.ellipse(
          s.x + s.w / 2,
          s.y + s.h / 2,
          Math.abs(s.w / 2),
          Math.abs(s.h / 2),
          0,
          0,
          Math.PI * 2,
        );
        ctx.stroke();
        break;
      }
      case "arrow": {
        ctx.strokeStyle = s.color;
        ctx.fillStyle = s.color;
        ctx.lineWidth = s.width;
        ctx.beginPath();
        ctx.moveTo(s.x1, s.y1);
        ctx.lineTo(s.x2, s.y2);
        ctx.stroke();
        const ang = Math.atan2(s.y2 - s.y1, s.x2 - s.x1);
        const head = 8 + s.width * 2.2;
        ctx.beginPath();
        ctx.moveTo(s.x2, s.y2);
        ctx.lineTo(
          s.x2 - head * Math.cos(ang - Math.PI / 7),
          s.y2 - head * Math.sin(ang - Math.PI / 7),
        );
        ctx.lineTo(
          s.x2 - head * Math.cos(ang + Math.PI / 7),
          s.y2 - head * Math.sin(ang + Math.PI / 7),
        );
        ctx.closePath();
        ctx.fill();
        break;
      }
      case "pencil": {
        if (s.points.length < 1) break;
        ctx.strokeStyle = s.color;
        ctx.lineWidth = s.width;
        ctx.beginPath();
        ctx.moveTo(s.points[0].x, s.points[0].y);
        for (let i = 1; i < s.points.length; i++) ctx.lineTo(s.points[i].x, s.points[i].y);
        ctx.stroke();
        break;
      }
      case "text": {
        ctx.fillStyle = s.color;
        ctx.textBaseline = "top";
        ctx.font = `${s.size}px system-ui, "Segoe UI", sans-serif`;
        s.text.split("\n").forEach((line, i) => {
          ctx.fillText(line, s.x, s.y + i * s.size * 1.2);
        });
        break;
      }
      case "number": {
        const r = 10 + s.width;
        ctx.fillStyle = s.color;
        ctx.beginPath();
        ctx.arc(s.x, s.y, r, 0, Math.PI * 2);
        ctx.fill();
        ctx.fillStyle = "#ffffff";
        ctx.textAlign = "center";
        ctx.textBaseline = "middle";
        ctx.font = `bold ${r}px system-ui, sans-serif`;
        ctx.fillText(String(s.n), s.x, s.y + 1);
        ctx.textAlign = "start";
        break;
      }
      case "mosaic": {
        const img = imgRef.current;
        if (!img || s.points.length === 0) break;
        const { x: sx, y: sy } = scaleRef.current;
        const r = s.width / 2; // 笔刷半径（CSS px）
        let minX = Infinity;
        let minY = Infinity;
        let maxX = -Infinity;
        let maxY = -Infinity;
        for (const p of s.points) {
          minX = Math.min(minX, p.x - r);
          minY = Math.min(minY, p.y - r);
          maxX = Math.max(maxX, p.x + r);
          maxY = Math.max(maxY, p.y + r);
        }
        const bw = maxX - minX;
        const bh = maxY - minY;
        if (bw <= 0 || bh <= 0) break;
        // 降采样再放大 = 像素化：临时画布尺寸按物理块大小
        const block = 12; // 每个马赛克块的物理像素边长
        const tw = Math.max(1, Math.round((bw * sx) / block));
        const th = Math.max(1, Math.round((bh * sy) / block));
        const tmp = document.createElement("canvas");
        tmp.width = tw;
        tmp.height = th;
        const tctx = tmp.getContext("2d");
        if (!tctx) break;
        tctx.drawImage(img, minX * sx, minY * sy, bw * sx, bh * sy, 0, 0, tw, th);
        // 裁剪到笔刷轨迹（一串圆叠加），只在划过处铺像素化块
        ctx.save();
        ctx.beginPath();
        for (const p of s.points) {
          ctx.moveTo(p.x + r, p.y);
          ctx.arc(p.x, p.y, r, 0, Math.PI * 2);
        }
        ctx.clip();
        const prevSmooth = ctx.imageSmoothingEnabled;
        ctx.imageSmoothingEnabled = false;
        ctx.drawImage(tmp, 0, 0, tw, th, minX, minY, bw, bh);
        ctx.imageSmoothingEnabled = prevSmooth;
        ctx.restore();
        break;
      }
    }
  }, []);

  useLayoutEffect(() => {
    const cvs = canvasRef.current;
    if (!cvs || !ready) return;
    const ctx = cvs.getContext("2d");
    if (!ctx) return;
    const { x: sx, y: sy } = scaleRef.current;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, cvs.width, cvs.height);
    ctx.setTransform(sx, 0, 0, sy, 0, 0); // 之后按 CSS px 绘制
    ctx.save();
    if (selection && committed) {
      ctx.beginPath();
      ctx.rect(selection.x, selection.y, selection.w, selection.h);
      ctx.clip();
    }
    for (const s of shapes) drawShape(ctx, s);
    if (draft) drawShape(ctx, draft);
    ctx.restore();
  }, [shapes, draft, selection, committed, ready, drawShape]);

  // ---- 合成「选区那块背景 + 标注」为最终 PNG（base64，无 data 前缀）----
  const composePng = useCallback((): string | null => {
    const sel = selection;
    const img = imgRef.current;
    if (!sel || !committed || !img) return null;
    if (sel.w < 1 || sel.h < 1) return null;
    const { x: sx, y: sy } = scaleRef.current;
    const physW = Math.max(1, Math.round(sel.w * sx));
    const physH = Math.max(1, Math.round(sel.h * sy));
    const out = document.createElement("canvas");
    out.width = physW;
    out.height = physH;
    const octx = out.getContext("2d");
    if (!octx) return null;
    // 背景选区那块
    octx.drawImage(img, sel.x * sx, sel.y * sy, sel.w * sx, sel.h * sy, 0, 0, physW, physH);
    // 叠加标注 canvas 对应区域（其像素坐标即物理坐标）
    const ac = canvasRef.current;
    if (ac) {
      octx.drawImage(ac, sel.x * sx, sel.y * sy, sel.w * sx, sel.h * sy, 0, 0, physW, physH);
    }
    const url = out.toDataURL("image/png");
    return url.slice(url.indexOf(",") + 1);
  }, [selection, committed]);

  // 选区左上角的全局物理坐标（boundsRef 为虚拟屏原点）
  const selOrigin = useCallback(() => {
    const sel = selection;
    const { x: sx, y: sy } = scaleRef.current;
    const b = boundsRef.current;
    return {
      x: sel ? Math.round(sel.x * sx) + b.x : b.x,
      y: sel ? Math.round(sel.y * sy) + b.y : b.y,
    };
  }, [selection]);

  // 操作反馈条：错误留久一点（用户得看清并决定重试还是退出），成功一闪即过。
  const showFlash = useCallback((kind: "ok" | "err", text: string) => {
    setFlash({ kind, text });
    if (flashTimerRef.current !== null) window.clearTimeout(flashTimerRef.current);
    flashTimerRef.current = window.setTimeout(() => setFlash(null), kind === "err" ? 4500 : 1400);
  }, []);

  // ---- 完成：合成后落盘/复制并关闭 ----
  // 保存前确认（settings.capture_confirm_save，默认开）：先把产物落到默认目录拿到路径，
  // 再弹确认条让用户「快速保存 / 另存为… / 删除」。用户 2026-09-21 反馈
  // 「录屏和截屏还有 gif 不能切换保存地址，应该像 PixPin 有个确认阶段」。
  const finish = useCallback(
    async (save: boolean, copy: boolean) => {
      if (busy) return;
      const pngBase64 = composePng();
      if (!pngBase64) return;
      // 确认模式只对「要落盘」的动作生效：保存/完成/Enter(auto_save) 弹确认条；
      // 复制按钮（finish(false, true)）不落盘也不弹条——用户 2026-09-24 反馈
      // 「点击复制时不用弹出保存地址」。旧逻辑无条件 `save: true` 还和
      // capture_auto_save=false 打架（关了自动保存照样落盘）。
      const confirm = settingsRef.current.capture_confirm_save && save;
      setBusy(true);
      try {
        const r = await invoke<{ path: string | null }>("finish_capture", {
          pngBase64,
          copy,
          save: confirm ? true : save,
        });
        const path = r?.path ?? null;
        if (confirm) {
          if (!path) {
            showFlash("err", "保存失败：后端没有返回产物路径");
            return; // 留在 overlay，让用户看见失败
          }
          // kind 按**真实扩展名**给：save_format_png=false 时产物是 .jpg，
          // 确认条文案与「另存为」的过滤器都要跟着变。
          const ext = path.split(".").pop()?.toLowerCase();
          setSavePrompt({ kind: ext === "jpg" || ext === "jpeg" ? "jpg" : "png", path });
          return; // 留在 overlay 等用户确认，不关窗
        }
        if (save && path) {
          // 没有确认条时后端只写了 .pending 暂存件（落盘顺序改成「先暂存、确认再进
          // 产物目录」），必须在关窗前提交，否则产物永远躺在暂存区。
          await invoke<string>("commit_capture", { path });
        }
      } catch (e) {
        // 落盘/复制失败必须让用户看见：原来只 console.error，界面停在原地
        // 既不弹条也不给提示，用户只会以为按钮没点到（D12）。
        console.error("finish_capture 失败", e);
        showFlash("err", `保存失败：${errText(e)}`);
        return; // 不关窗：用户看得见错误，也能重试
      } finally {
        setBusy(false);
      }
      if (copy) {
        // 复制成功也要有可见反馈（D8）：工具条「复制」此前一声不吭地关窗。
        showFlash("ok", "已复制到剪贴板");
        // 让反馈条被看见再关窗（不用 setTimeout 回调链，直接等这个 promise）。
        const { promise: flashed, resolve: flashDone } = Promise.withResolvers<void>();
        window.setTimeout(flashDone, 600);
        await flashed;
      }
      if (!confirm) await invoke("close_overlay");
    },
    [busy, composePng, showFlash],
  );

  // ---- 贴图：先收 overlay 再建 pin 窗 ----
  // 为什么不先 invoke("pin_image")：overlay 是全屏置顶窗，**它还在时建第三个 webview
  // 会把主线程堵死**（同 0.21「悬浮球在时新建 overlay 停在 about:blank」同族缺陷）——
  // 用户实测「点贴图后整个界面卡死、按钮能点但没反应，只能重启」。先 hide 再建窗。
  const doPin = useCallback(async () => {
    if (busy) return;
    const pngBase64 = composePng();
    if (!pngBase64) return;
    setBusy(true);
    const o = selOrigin();
    try {
      await invoke("close_overlay");
    } catch {
      /* 已关则忽略 */
    }
    try {
      await invoke("pin_image", { pngBase64, x: o.x, y: o.y });
    } catch (e) {
      console.error("pin_image 失败", e);
    }
  }, [busy, composePng, selOrigin]);

  // ---- 长截图：先收 overlay 再滚动拼接 ----
  // 为什么不先跑 capture_long：longshot 靠 SendInput 滚轮驱动**目标窗口**滚动，
  // 而 overlay 自己是全屏置顶窗，开着时滚轮全被它吃掉 → 目标不滚 → 第一帧即判到底
  // → 只出一帧就收回（2026-09-21 用户实测「亮一下就收回」）。必须先 hide overlay。
  // restoreBall=false：否则 close_overlay 会把悬浮球弹回屏幕上，球若落在选区里
  // 会被整帧抓屏拍进拼接产物（2026-09-21 审计发现）。
  // 拼接完成后同样进保存确认阶段（与截图/录屏一致）。
  const doLong = useCallback(async () => {
    if (busy || !selection) return;
    const { x: sx, y: sy } = scaleRef.current;
    const b = boundsRef.current;
    const bounds = {
      x: Math.round(selection.x * sx) + b.x,
      y: Math.round(selection.y * sy) + b.y,
      w: Math.round(selection.w * sx),
      h: Math.round(selection.h * sy),
    };
    setBusy(true);
    try {
      await invoke("close_overlay", { restoreBall: false });
    } catch {
      /* 已关则忽略 */
    }
    try {
      const r = await invoke<{ path: string | null }>("capture_long", {
        bounds,
        copy: true,
        save: true,
      });
      if (settingsRef.current.capture_confirm_save && r?.path) {
        // overlay 已隐藏，但页面还活着——重新显示它来承载确认条。
        // keepPromptRef 告诉 overlay-shown 的重置逻辑「这次别清确认条」。
        keepPromptRef.current = true;
        await invoke("open_overlay");
        // 与 finish 同一条规则：kind 按真实扩展名给（save_format_png=false 时长截图也是 .jpg）。
        const lext = r.path.split(".").pop()?.toLowerCase();
        setSavePrompt({ kind: lext === "jpg" || lext === "jpeg" ? "jpg" : "png", path: r.path });
        return;
      }
      if (r?.path) {
        // 没有确认条时后端只写了 .pending 暂存件：得提交到产物目录，
        // 否则长截图永远躺在暂存区（D12 的落盘顺序）。
        await invoke<string>("commit_capture", { path: r.path });
      }
    } catch (e) {
      console.error("capture_long 失败", e);
      // overlay 此刻是关着的：错误必须把 overlay 重新拉起来才看得见，
      // 否则用户只看到「按了没反应」。
      await invoke("open_overlay").catch(() => {});
      showFlash("err", `长截图失败：${errText(e)}`);
    } finally {
      setBusy(false);
    }
  }, [busy, selection, showFlash]);

  // ---- 录制模式切换：切档同时把帧率就近落到该模式合法档位。----
  const changeRecMode = useCallback((m: "video" | "gif") => {
    setRecMode(m);
    setRecFps((f) => {
      const choices = m === "gif" ? REC_FPS_GIF : REC_FPS_VIDEO;
      if (choices.includes(f)) return f;
      return m === "gif" ? (f >= 24 ? 24 : f >= 15 ? 15 : 10) : f >= 30 ? 30 : 24;
    });
  }, []);

  // 录音频开关（D7）：录屏入口以前没有音频选项，用户只能去设置页翻。
  // 后端 start_record 只从配置文件读 record_audio / record_audio_device（不收命令参数），
  // 所以这里写配置而不是传参；写前重拉一次最新配置，避免把设置窗口刚改的值覆盖回去。
  const toggleRecAudio = useCallback(async () => {
    const next = !recAudio;
    setRecAudio(next);
    try {
      const cur = await invoke<Settings>("get_settings");
      await invoke("update_settings", { newCfg: { ...DEFAULTS, ...cur, record_audio: next } });
      setSettings((s) => ({ ...s, record_audio: next }));
    } catch (e) {
      setRecAudio(!next);
      showFlash("err", `音频设置失败：${errText(e)}`);
    }
  }, [recAudio, showFlash]);

  // ---- 录制：把选区物理边界 + 帧率/码率 + 是否 GIF 交给 start_record
  // （后端 ddagrab 抓该区域；GIF 模式先录 MP4、停止时 palettegen/paletteuse 转成真 GIF）。
  // GIF 帧率封顶 24（GIF 格式与体积的现实上限）。
  const startRecord = useCallback(
    async (mode: "video" | "gif") => {
      if (busy || !selection || recording) return;
      const { x: sx, y: sy } = scaleRef.current;
      const b = boundsRef.current;
      const bounds = {
        x: Math.round(selection.x * sx) + b.x,
        y: Math.round(selection.y * sy) + b.y,
        w: Math.round(selection.w * sx),
        h: Math.round(selection.h * sy),
      };
      const fps = mode === "gif" ? Math.min(recFps, 24) : recFps;
      // 进入录制态前收起工具/取景辅助，冻结帧与遮罩隐藏 → 透明窗口露出实时桌面供抓录。
      setTool(null);
      cursorRef.current.on = false; // 放大镜直接隐藏（rAF 循环下一帧生效）
      hoverWinRef.current = null;
      hoverLabelRef.current = null;
      // A+（2026-09-24）先置录制态再 invoke：让「工具条/尺寸标签隐藏、红框外扩、
      // 录制条换边」先于 ffmpeg 起流落地（React 提交远早于首帧百 ms 级），起流
      // 窗口期成品里不再有截图 UI。失败回滚 setRecording(null)，不留假录制态。
      setRecElapsed(0);
      setRecording({ mode, startedAt: Date.now(), pausedAt: null });
      try {
        await invoke<string>("start_record", {
          bounds,
          fps,
          bitrateK: recBitrate,
          gif: mode === "gif",
        });
      } catch (e) {
        console.error("start_record 失败", e);
        setRecording(null);
        showFlash("err", `录制启动失败：${errText(e)}`);
      }
    },
    [busy, selection, recording, recFps, recBitrate, showFlash],
  );

  // ---- 统一开录入口：先按 record_countdown 倒计时，归零再真正开录 ----
  // 谁在什么时机调它：① 工具条「开始录制」按钮；② G 键；③ 热键路径——后端
  // `trigger_capture` / `toggle_record` 的武装分支 emit `snappin-arm-start`，本页监听到就调它。
  // 后端**不**直接 start_record，倒计时与「选区/模式/帧率/码率」都在前端这一份 state 里，
  // 两处各存一份参数迟早漂移。record_countdown=0 时同步直落 startRecord（与改造前行为一致）。
  const beginRecord = useCallback(
    (mode: "video" | "gif" = recMode) => {
      // 倒计时进行中再按一次不重入（热键连按会走到这里）。
      if (busy || !selection || recording || countdown !== null) return;
      const secs = Math.max(0, Math.min(10, Math.floor(settingsRef.current.record_countdown || 0)));
      if (secs <= 0) {
        void startRecord(mode);
        return;
      }
      setCountdown({ secs, mode });
    },
    [busy, selection, recording, countdown, recMode, startRecord],
  );

  // 倒计时每秒减一，归零后真正开录。effect 依赖 countdown 对象本身，每 tick 重排一次定时器。
  useEffect(() => {
    if (!countdown) return;
    if (countdown.secs <= 0) {
      const mode = countdown.mode;
      setCountdown(null);
      void startRecord(mode);
      return;
    }
    const id = window.setTimeout(() => {
      setCountdown((c) => (c ? { ...c, secs: c.secs - 1 } : c));
    }, 1000);
    return () => window.clearTimeout(id);
  }, [countdown, startRecord]);

  // ---- 停止录制：stop_record 收尾产物 → 确认条（或直接关闭 overlay）。----
  // 收尾（含 GIF 转码）在后端后台线程跑，这里 await 期间界面仍可响应
  // （2026-09-21：改 async 前它同步堵主线程，用户看到的是「卡死」）。
  const stopRecord = useCallback(async () => {
    if (!recording) return;
    const mode = recording.mode;
    setBusy(true);
    // 立即进入收尾态：冻结计时并挡住暂停/停止按钮，避免「按了停止计时还在涨」。
    setStopping(true);
    try {
      const p = await invoke<string>("stop_record");
      setRecording(null);
      if (settingsRef.current.capture_confirm_save && p) {
        setSavePrompt({ kind: mode === "gif" ? "gif" : "mp4", path: p });
        return; // 留在 overlay 等用户确认
      }
    } catch (e) {
      console.error("stop_record 失败", e);
    } finally {
      setRecording(null);
      setStopping(false);
      setBusy(false);
    }
    await invoke("close_overlay");
  }, [recording]);

  // ---- 保存确认条三个动作（见 savePrompt 状态注释）----
  // 快速保存：把暂存件提交到最终目录（后端 D12：finish_capture 只写 .pending 暂存件，
  // 真正进产物目录是 commit_capture 这一步；录制产物已是最终路径，commit 原样返回）。
  const confirmKeep = useCallback(async () => {
    const p = savePrompt;
    setSavePrompt(null);
    if (!p) {
      await invoke("close_overlay");
      return;
    }
    try {
      await invoke<string>("commit_capture", { path: p.path });
    } catch (e) {
      console.error("commit_capture 失败", e);
      showFlash("err", `保存失败：${errText(e)}`);
      setSavePrompt(p); // 留在确认条上，用户能看见失败并重试
      return;
    }
    await invoke("close_overlay");
  }, [savePrompt, showFlash]);

  // 另存为…：原生对话框选目标 → 复制过去 → 删掉默认路径那份（等价移动）。
  const confirmSaveAs = useCallback(async () => {
    const p = savePrompt;
    // 重入闸门：对话框开着时重复点击直接忽略（0.26 问题 2）。
    if (!p || busy) return;
    // 另存为对话框弹出前停掉回放：文件要被移动，边播边搬会读到半途失败。
    videoRef.current?.pause();
    setBusy(true);
    const name = p.path.split(/[\\/]/).pop() ?? "";
    let chosen: string | null = null;
    try {
      chosen = await invoke<string | null>("pick_save_path", { defaultName: name, kind: p.kind });
    } catch (e) {
      console.error("pick_save_path 失败", e);
      showFlash("err", `另存为失败：${errText(e)}`);
      setBusy(false);
      return; // 留在确认条上让用户重试
    }
    if (chosen && chosen !== p.path) {
      try {
        await invoke("save_file_as", { src: p.path, dst: chosen });
        await invoke("remove_file", { path: p.path });
      } catch (e) {
        console.error("save_file_as 失败", e);
        showFlash("err", `另存为失败：${errText(e)}`);
        setBusy(false);
        return; // 留在确认条上让用户重试
      }
    }
    // 无论成功取消，闸门都要复位，否则下一次另存为会被永久挡住。
    setBusy(false);
    if (!chosen) {
      // 用户取消对话框（0.26 验收判据 3）：产物留在默认路径、确认条仍在，
      // 不关 overlay——关掉等于逼用户重截一次。
      return;
    }
    setSavePrompt(null);
    await invoke("close_overlay");
  }, [savePrompt, busy, showFlash]);

  // 删除：丢弃产物（暂存件）并收工。
  const confirmDiscard = useCallback(async () => {
    const p = savePrompt;
    setSavePrompt(null);
    if (p) {
      try {
        await invoke("remove_file", { path: p.path });
      } catch (e) {
        console.error("remove_file 失败", e);
        showFlash("err", `删除失败：${errText(e)}`);
        setSavePrompt(p); // 留在确认条上让用户重试
        return;
      }
    }
    await invoke("close_overlay");
  }, [savePrompt, showFlash]);

  // 复制：把产物送进系统剪贴板（后端 copy_product：图片走 CF_DIB、gif/mp4 走
  // CF_HDROP 文件列表）。**不关保存条**——用户常常先复制再决定另存/删除。
  // 2026-10-14 用户报「弹出的图不能复制，只有保存栏」→ 这条补的就是它。
  const copyProduct = useCallback(async () => {
    const p = savePrompt;
    if (!p || busy) return;
    setBusy(true);
    try {
      await invoke("copy_product", { path: p.path });
      setCopiedFlash(true);
      window.setTimeout(() => setCopiedFlash(false), 1200);
    } catch (e) {
      console.error("复制产物失败", e);
      showFlash("err", `复制失败：${errText(e)}`);
    } finally {
      setBusy(false);
    }
  }, [savePrompt, busy]);

  // ---- 取消录制：discard=true 丢弃产物不保存 → 关闭 overlay。----
  const discardRecord = useCallback(async () => {
    if (!recording) return;
    setStopping(true);
    try {
      await invoke("stop_record", { discard: true });
    } catch (e) {
      console.error("stop_record(discard) 失败", e);
    } finally {
      setRecording(null);
      setStopping(false);
      await invoke("close_overlay");
    }
  }, [recording]);

  // 录制计时：每 250ms 刷新已录秒数。**暂停中不再走时**（以 pausedAt 冻结）；
  // **停止收尾期（stopping）同样冻结**——否则停止到产物弹出之间计时还在涨，
  // 用户会以为没停住（2026-10-14 报障）。
  useEffect(() => {
    if (!recording) return;
    // 暂停/收尾态结算一次（pausedAt 冻结）；运行态每 250ms 用当前时刻刷新。
    setRecElapsed(Math.floor(((recording.pausedAt ?? Date.now()) - recording.startedAt) / 1000));
    if (recording.pausedAt !== null || stopping) return;
    const id = window.setInterval(
      () => setRecElapsed(Math.floor((Date.now() - recording.startedAt) / 1000)),
      250,
    );
    return () => window.clearInterval(id);
  }, [recording, stopping]);

  // ---- 录制态输入权让渡（2026-09-24 用户报障「录屏时我应该可以使用键盘，这是传统
  // 录屏软件都行的。gif也是」）：后端把录制条矩形外做成 HTTRANSPARENT（鼠标穿透到
  // 底下应用、红框视觉保留）+ WS_EX_NOACTIVATE（点录制条不夺焦）+ 归还原前台窗
  // （键盘回用户程序）。recbar 位置随选区越界翻转/暂停文案宽度变化 → 1s 轻推兜底；
  // 停止时推 null 复原并回焦 overlay（保存条 Esc 语义不变）。
  const recbarRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const send = (rects: {
      bar: { x: number; y: number; w: number; h: number };
      frame: { x: number; y: number; w: number; h: number };
      inset: number;
    } | null) => {
      void invoke("rec_input_passthrough", { rects }).catch((e) =>
        console.error("输入权让渡失败", e),
      );
    };
    if (!recording) {
      send(null);
      return;
    }
    // 区域 = 录制条矩形 ∪ 红框环（外框−按 border 宽内缩挖洞 → 选区内部照常穿透）。
    // 红框/录制条位置随选区固定但暂停文案会改宽 → 1s 轻推兜底。
    const push = () => {
      const el = recbarRef.current;
      const fr = document.querySelector(".overlay-recframe");
      if (!el || !fr) return;
      const b = el.getBoundingClientRect();
      const f = fr.getBoundingClientRect();
      const inset = parseFloat(getComputedStyle(fr).borderTopWidth) || 0;
      send({
        bar: { x: b.x, y: b.y, w: b.width, h: b.height },
        frame: { x: f.x, y: f.y, w: f.width, h: f.height },
        inset,
      });
    };
    push();
    const id = window.setInterval(push, 1000);
    return () => window.clearInterval(id);
  }, [recording]);

  // ---- 暂停/继续录制：后端分段（pause 封存当前段、resume 新起一段、stop 无损拼接）----
  // 前端只做两件事：调命令 + 维护计时基准（继续时把 startedAt 前移，已录时长才连续）。
  // 状态同步抽成 applyPause：录制条按钮与全局「暂停/继续」热键（后端 `snappin-rec-paused`
  // 事件）两条入口共用，避免两处各写一遍计时基准换算。
  const applyPause = useCallback((paused: boolean) => {
    setRecording((r) => {
      if (!r) return r;
      if (paused === (r.pausedAt !== null)) return r;
      if (paused) return { ...r, pausedAt: Date.now() };
      return {
        ...r,
        pausedAt: null,
        startedAt: r.startedAt + (Date.now() - (r.pausedAt ?? Date.now())),
      };
    });
  }, []);

  const togglePauseRecord = useCallback(async () => {
    if (!recording) return;
    const paused = recording.pausedAt !== null;
    try {
      await invoke(paused ? "resume_record" : "pause_record");
    } catch (e) {
      console.error("暂停/继续录制失败", e);
      return;
    }
    applyPause(!paused);
  }, [recording, applyPause]);

  // 暂停热键（后端 `record_pause` 动作）在后台线程调 pause/resume，前端看不到状态，
  // 只能靠事件同步——否则热键按了暂停、条上计时照走（用户 2026-10-14 报的就是这类错觉）。
  useEffect(() => {
    const un = listen<{ paused: boolean }>("snappin-rec-paused", (ev) => {
      applyPause(!!ev.payload?.paused);
    });
    return () => {
      void un.then((f) => f());
    };
  }, [applyPause]);

  // ---- OCR：合成选区 → 识别 → 文本进剪贴板 + 结果浮层（不关 overlay）----
  const doOcr = useCallback(async () => {
    if (busy) return;
    const pngBase64 = composePng();
    if (!pngBase64) return;
    setBusy(true);
    try {
      const res = await invoke<{ text: string; lines: unknown[] }>("ocr_image", { pngBase64 });
      const text = res?.text ?? "";
      // 复制**由后端按 settings.ocr_auto_copy 做**（commands.rs::auto_copy_ocr）：前端原来
      // 无条件 writeText，开关关掉也照样进剪贴板——那这个设置就是假开关。这里只如实回显结果。
      setOcrCopied(text.length > 0 && settingsRef.current.ocr_auto_copy);
      setOcrText(text.length ? text : "（未识别到文字）");
    } catch (e) {
      console.error("ocr_image 失败", e);
      setOcrText(`OCR 失败：${String(e)}`);
    } finally {
      setBusy(false);
    }
  }, [busy, composePng]);

  const commitText = useCallback(() => {
    if (!textEdit) return;
    const v = textValue.trim();
    if (v) {
      const size = 14 + width * 3;
      setShapes((prev) => [
        ...prev,
        { kind: "text", x: textEdit.x, y: textEdit.y, text: textValue, color, size },
      ]);
      setRedo([]);
    }
    setTextEdit(null);
    setTextValue("");
  }, [textEdit, textValue, color, width]);

  // 「检测文本并复制」（Shift+C，PixPin 同款）：把光标下最内层的文字元素取出来——
  // 元素自带 Name 就直接用，否则把该元素矩形交给后端 OCR；结果进剪贴板并用 OCR 浮层展示。
  // 为什么单独一条：常态悬停刻意跳过文字级（见 resolveHover），文字要按需取。
  const copyTextAtCursor = useCallback(async () => {
    const p = lastPointerRef.current;
    if (!p) return;
    let chain = uiaChainRef.current;
    if (!chain.length) {
      try {
        chain =
          (await invoke<ElementInfo[]>("ui_chain_at", {
            x: Math.round(p.x),
            y: Math.round(p.y),
          })) ?? [];
      } catch {
        chain = [];
      }
    }
    const el = chain.find((e) => TEXT_KINDS.includes(e.kind)) ?? chain[0];
    if (!el) {
      setOcrText("（光标处没识别到文字）");
      return;
    }
    // 优先 OCR 该元素的屏幕区域（用户要的是**看得见的文字**）；UIA 的 Name 对
    // 文档类元素往往只是控件名（实测记事本给的是「文本编辑器」），只能当兜底。
    let text = "";
    try {
      const res = await invoke<{ text: string }>("ocr_region", {
        bounds: { x: el.x, y: el.y, w: el.w, h: el.h },
      });
      text = (res?.text ?? "").trim();
    } catch (e) {
      setOcrText(`检测文本失败：${String(e)}`);
      return;
    }
    if (!text) text = el.name.trim();
    if (!text) {
      setOcrText("（未识别到文字）");
      setOcrCopied(false);
      return;
    }
    // Shift+C 这个动作本身就是「检测文本**并复制**」（左下角快捷键卡这么承诺的），
    // 所以这里保留显式复制：`ocr_auto_copy` 管的是「OCR 结果自动进剪贴板」这条便利路径，
    // 不该让用户关掉它以后连 Shift+C 都变成哑巴。后端识别时若开关开着也已复制过，重复无副作用。
    try {
      await navigator.clipboard.writeText(text);
      setOcrCopied(true);
    } catch {
      /* 剪贴板不可用仍展示 */
      setOcrCopied(false);
    }
    setOcrText(text);
  }, []);

  // ---- 左下角快捷键提示卡（PixPin 同款形态，docs/UI设计规范.md §1.5）----
  // 只列**真实可用**的键：设置面板曾经列过 L/G/Ctrl+T/Shift+C 而代码没实现，
  // 属于「界面承诺、代码不兑现」（用户 2026-09-21 因此以为功能丢了）。这张卡
  // 就是可用键的权威展示，加键/删键都要同步改这里。
  // 回退值必须与后端 hotkeys::DEFAULT_CAPTURE（= 设置页 HOTKEY_ACTIONS 的 def）一致：
  // hotkeys.capture 为空/缺失时后端注册的就是 Ctrl+1，这里写别的键就是提示卡撒谎。
  const captureKey = settings.hotkeys?.capture || "Ctrl+1";
  // 暂停/继续的全局热键（后端动作 record_pause）：默认不占键，绑了才可能收到。
  // **录制中页面收不到键盘**：0.36 起录制态把键盘让渡回用户程序（rec_input_passthrough
  // + WS_EX_NOACTIVATE），所以这里不能列 Esc/空格（列了就是 09-24 前的老承诺）。
  const pauseKey = settings.hotkeys?.record_pause || "";
  const hintRows = useMemo<[string, string][]>(() => {
    if (recording)
      return [
        [captureKey, stopping ? "正在写出产物…" : "停止录制"],
        [pauseKey || "录制条 ‖", "暂停 / 继续录制"],
      ];
    // 倒计时阶段只认 Esc（其余键被 onKey 吞掉），提示卡必须只说真话。
    if (countdown) return [["Esc / 点击", "取消倒计时"]];
    // 保存确认阶段：只留确认条相关键（PixPin 同款「确认阶段」心智）；
    // mp4 多列回放键——空格绑定见 onKey，进度/音量/倍速/全屏由原生控制条自带
    //（用户 2026-09-24 点名「不能只做一个开始和暂停键」）。
    if (savePrompt)
      return savePrompt.kind === "mp4"
        ? [
            ["空格", "播放 / 暂停"],
            ["Enter", "快速保存"],
            ["Esc", "删除并退出"],
          ]
        : [
            ["Enter", "快速保存"],
            ["Esc", "删除并退出"],
          ];
    // 选区已定（committed）：滚轮与 Tab 在这个阶段是死键——onWheel 开头就
    // `if (committed || recording || tool !== null) return`，Tab 分支要求 `!committed`，
    // 且 onPointerMove 的 UIA 命中链只在未提交时更新（没有链可切）。所以这里**不能**
    // 列它们，否则又是「界面承诺、代码不兑现」。要恢复这两条得先让提交态也能换层。
    if (committed && selection)
      return [
        ["Shift+C", "检测文本并复制"],
        [captureKey, recMode === "gif" ? "开始录制 GIF" : "开始录制视频"],
        ["L", "长截图"],
        ["Ctrl+T", "贴图"],
        ["Ctrl+Z / Ctrl+Y", "撤销 / 重做"],
        ["Ctrl+Shift+Z", "清空标注"],
        // auto_save 关掉时 Enter 只复制不落盘（finish 的 confirm 也已按 save 生效），
        // 提示卡必须跟着说实话。
        ["Enter", settings.capture_auto_save ? "保存并复制" : "复制"],
        ["Esc / 右键", "关闭 / 返回"],
      ];
    return [
      ["鼠标滚轮", "切换识别层级"],
      ["Tab", "切换元素/窗口"],
      ["Shift+C", "检测文本并复制"],
      ["右键", "取消"],
      ["Esc", "关闭"],
    ];
  }, [recording, stopping, pauseKey, countdown, committed, selection, recMode, captureKey, savePrompt, settings.capture_auto_save]);

  // ---- overlay 复用：每次显示时重置状态并重抓冻结帧 ----
  // 后端 close_overlay 只隐藏窗口（反复建窗会让 webview 停在 about:blank），
  // 所以「再次截图」拿到的是同一个页面——这里必须自己把上一次的状态清干净。
  useEffect(() => {
    const un = listen("overlay-shown", () => {
      setSelection(null);
      setCommitted(false);
      setShapes([]);
      setRedo([]);
      setDraft(null);
      setTool(null);
      setTextEdit(null);
      setTextValue("");
      setOcrText(null);
      setOcrCopied(false);
      setRecording(null);
      setTranscode(null);
      setCountdown(null);
      // 长截图重开 overlay 那一次要保住确认条（见 keepPromptRef 注释）。
      if (keepPromptRef.current) keepPromptRef.current = false;
      else setSavePrompt(null);
      setBusy(false);
      setGridOn(false);
      setReady(false);
      // 旧冻结帧必须先撤下：窗口此刻还不可见（后端不 show），撤下是为了 show 时不会有一帧旧图。
      setDataUrl("");
      // 工具条拖动位置是「本次会话」的：新一次截图回到选区下方默认位。
      setBarPos(null);
      recogModeRef.current = "element";
      uiaChainRef.current = [];
      uiaLevelRef.current = 0;
      uiaKeyRef.current = null;
      uiaInflightRef.current = false;
      hoverWinRef.current = null;
      hoverLabelRef.current = null;
      hoverKeyRef.current = null;
      hoverPendingRef.current = null;
      pendingWinRef.current = null;
      gestureRef.current = null;
      lastPointerRef.current = null;
      cursorRef.current = { x: 0, y: 0, on: false };
      // 设置可能在上次显示之后被改过（尤其热键重绑），重拉一次再渲染提示卡。
      void loadSettings();
      // 抓新冻结帧 → 抓完才亮窗（窗口在此之前保持不可见，见 grabFrame 注释）。
      void grabFrame();
      void invoke("overlay_ready").catch(() => {});
    });
    return () => {
      void un.then((f) => f());
    };
  }, [loadSettings, grabFrame]);

  // ---- 武装录制参数：热键要能在「选区已定」时直接开录 ----
  // 选区边界只有前端知道，所以在进入可录态时把参数推给后端（arm_record），
  // 离开时收回（disarm_record）。热键处理器据此把「截图键」当录制开关用。
  useEffect(() => {
    if (!committed || !selection || recording || busy) return;
    const { x: sx, y: sy } = scaleRef.current;
    const b = boundsRef.current;
    void invoke("arm_record", {
      info: {
        bounds: {
          x: Math.round(selection.x * sx) + b.x,
          y: Math.round(selection.y * sy) + b.y,
          w: Math.round(selection.w * sx),
          h: Math.round(selection.h * sy),
        },
        fps: recMode === "gif" ? Math.min(recFps, 24) : recFps,
        bitrateK: recBitrate,
        gif: recMode === "gif",
      },
    }).catch(() => {});
    return () => {
      void invoke("disarm_record").catch(() => {});
    };
  }, [committed, selection, recording, busy, recMode, recFps, recBitrate]);

  // ---- GIF 转码进度：后端 `Recorder::stop` 在 palettegen/paletteuse 阶段推百分比 ----
  useEffect(() => {
    const un = listen<{ active: boolean; percent: number }>("snappin-transcode", (ev) => {
      setTranscode(ev.payload?.active ? Math.max(0, Math.min(99, ev.payload.percent | 0)) : null);
    });
    return () => {
      void un.then((f) => f());
    };
  }, []);

  // ---- 热键发起的录制：后端起停后推事件，页面据此同步录制条 ----
  useEffect(() => {
    const un = listen<{ recording: boolean; gif?: boolean; path?: string }>(
      "snappin-record",
      (ev) => {
        if (ev.payload?.recording) {
          setRecElapsed(0);
          setStopping(false);
          setRecording({ mode: ev.payload.gif ? "gif" : "video", startedAt: Date.now(), pausedAt: null });
          cursorRef.current.on = false;
          hoverWinRef.current = null;
          hoverLabelRef.current = null;
        } else {
          // 热键停止也走这里：收尾完成才推事件，先清收尾态再决定留 overlay 还是关。
          setStopping(false);
          setRecording(null);
          const p = ev.payload?.path;
          // 热键停止（后端后台线程收尾，完成才推 path）：开着确认条时留在 overlay
          // 让用户选保存位置；否则照旧直接关。
          if (p && settingsRef.current.capture_confirm_save) {
            const ext = p.split(".").pop()?.toLowerCase();
            setSavePrompt({ kind: ext === "gif" ? "gif" : "mp4", path: p });
            return;
          }
          void invoke("close_overlay");
        }
      },
    );
    return () => {
      void un.then((f) => f());
    };
  }, []);

  // ---- 热键发起的开录：后端 `trigger_capture` / `toggle_record` 的武装分支 emit
  // `snappin-arm-start`（不带参数——选区/模式/帧率/码率都在本页 state 里），走统一入口
  // `beginRecord()`，于是 record_countdown 倒计时对热键与按钮一视同仁。
  // 用 ref 取最新的 beginRecord：监听只订阅一次，直接闭包捕获会永远看到挂载那一刻的
  // state（selection=null），热键按下去就毫无反应。
  const beginRecordRef = useRef(beginRecord);
  useEffect(() => {
    beginRecordRef.current = beginRecord;
  }, [beginRecord]);
  useEffect(() => {
    const un = listen("snappin-arm-start", () => beginRecordRef.current());
    return () => {
      void un.then((f) => f());
    };
  }, []);

  // ---- 全局快捷键 ----
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // 原生全屏看片中：Esc/方向键归浏览器（退出全屏），绝不能落进下面的
      // 「Esc=删除并退出」——否则看片按 Esc 退全屏会把刚录的文件删了。
      if (document.fullscreenElement) return;
      const el = document.activeElement;
      if (el && el.tagName === "TEXTAREA") return; // 文字编辑中，交给输入框
      // 焦点在回放播放器或保存条按钮上：空格/回车/方向键交给原生
      //（Chromium 按钮 Enter/Space=点击、<video controls> 自己收键盘）；
      // Esc 例外——非全屏时仍是「删除并退出」，与提示卡承诺一致。
      if (
        e.key !== "Escape" &&
        el &&
        typeof el.closest === "function" &&
        el.closest(".overlay-savepreview, .overlay-savebar")
      ) {
        return;
      }
      const key = e.key.toLowerCase();
      const mod = e.ctrlKey || e.metaKey;
      // 倒计时中：只认 Esc（取消），其余键全部吞掉。否则 Enter/Ctrl+S 会在倒计时里把截图
      // 落盘，而倒计时归零后又照常开录——等于凭空多存一份用户没要的图。
      if (countdown) {
        e.preventDefault();
        if (e.key === "Escape") setCountdown(null);
        return;
      }
      // 录制中：Esc/Enter 停止并收尾，空格暂停/继续，其余键忽略。
      if (recording) {
        if (e.key === "Escape" || e.key === "Enter") {
          e.preventDefault();
          void stopRecord();
        } else if (e.key === " ") {
          e.preventDefault();
          void togglePauseRecord();
        }
        return;
      }
      // 回放空格：保存确认阶段焦点不在播放器上时，空格=播放/暂停 mp4
      //（焦点在播放器上时上面已放行给原生，两路互斥不双触发）。
      if (savePrompt && e.key === " ") {
        const v = videoRef.current;
        if (v) {
          e.preventDefault();
          if (v.paused) void v.play();
          else v.pause();
        }
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        // 保存确认条打开时，Esc = 删除并收工（与确认条的「删除」同义），
        // 避免「按 Esc 关了窗却留下一个用户没确认要的文件」。
        if (savePrompt) void confirmDiscard();
        else invoke("close_overlay");
      } else if (e.shiftKey && key === "c") {
        // Shift+C：检测光标处文字并复制（设置面板「内置快捷键」页对外承诺的键）。
        e.preventDefault();
        void copyTextAtCursor();
      } else if (e.key === "Tab" && !committed) {
        // Tab：在「元素级识别」与「整窗识别」之间切换（PixPin 同款）。
        e.preventDefault();
        recogModeRef.current =
          recogModeRef.current === "element"
            ? "window"
            : recogModeRef.current === "window"
              ? "element"
              : "element";
        const last = lastPointerRef.current;
        if (last) resolveHover(last.x, last.y, recogModeRef.current !== "window", true);
      } else if (e.key === "Enter") {
        e.preventDefault();
        // 保存确认条打开时 Enter = 快速保存（与确认条主按钮同义）。
        if (savePrompt) void confirmKeep();
        else
          void finish(
            settingsRef.current.capture_auto_save,
            settingsRef.current.capture_copy_after,
          );
      } else if (mod && key === "s") {
        // Ctrl+S：工具条 tooltip 承诺「保存 (Ctrl+S)」，此前压根没接线 → 按了没反应（D12）。
        // 语义与工具条「保存」按钮一致：只落盘，不复制。确认条开着时等同「快速保存」。
        e.preventDefault();
        if (savePrompt) void confirmKeep();
        else void finish(true, false);
      } else if (key === "l" && !mod && !e.shiftKey && committed && selection) {
        // L：长截图（设置面板承诺的键）
        e.preventDefault();
        void doLong();
      } else if (key === "g" && !mod && !e.shiftKey && committed && selection) {
        // G：录制视频（设置面板承诺的键）——走统一入口，倒计时生效
        e.preventDefault();
        beginRecord("video");
      } else if (mod && e.shiftKey && key === "z") {
        // Ctrl+Shift+Z：清空全部标注（设置面板承诺的键）
        e.preventDefault();
        setShapes([]);
        setRedo([]);
        setDraft(null);
      } else if (mod && key === "t" && committed && selection) {
        // Ctrl+T：贴图（设置面板承诺的键）
        e.preventDefault();
        void doPin();
      } else if (mod && key === "z") {
        e.preventDefault();
        setShapes((prev) => {
          if (prev.length === 0) return prev;
          const last = prev[prev.length - 1];
          setRedo((r) => [...r, last]);
          return prev.slice(0, -1);
        });
      } else if (mod && key === "y") {
        e.preventDefault();
        setRedo((prev) => {
          if (prev.length === 0) return prev;
          const last = prev[prev.length - 1];
          setShapes((s) => [...s, last]);
          return prev.slice(0, -1);
        });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [
    finish,
    recording,
    countdown,
    stopRecord,
    togglePauseRecord,
    resolveHover,
    committed,
    selection,
    copyTextAtCursor,
    doLong,
    doPin,
    beginRecord,
    savePrompt,
    confirmDiscard,
    confirmKeep,
  ]);

  // ---- 指针交互（根节点统一处理）----

  // ---- 悬浮提示：自绘 tooltip（不用原生 title）----
  // 为什么不用 title：覆盖层是透明置顶窗，原生 tooltip 由浏览器画、字号小，
  // 且容易被窗口边界裁掉或被工具条/选区框挡住（用户 2026-09-21 实测反馈
  // 「鼠标移到图标显示的文字太小、很容易被遮住」）。自绘：15px 实底、z-index 最高、
  // 越界自动翻转/夹紧。命令式 DOM（同放大镜），hover 不触发 React 重渲染。
  const tipRef = useRef<HTMLDivElement | null>(null);
  const hideTip = useCallback(() => {
    const t = tipRef.current;
    if (t) t.style.display = "none";
  }, []);

  /// 「返回」：撤销当前选区与标注，退回自由框选态（不关 overlay）。
  /// 右键在已选中时走这里；语义同 PixPin 的返回上一步。
  const resetSelection = useCallback(() => {
    setSelection(null);
    setCommitted(false);
    setShapes([]);
    setRedo([]);
    setDraft(null);
    setTool(null);
    setTextEdit(null);
    setTextValue("");
    setGridOn(false);
    hoverWinRef.current = null;
    hoverLabelRef.current = null;
    hoverKeyRef.current = null;
    hoverPendingRef.current = null;
    pendingWinRef.current = null;
    gestureRef.current = null;
    uiaLevelRef.current = 0;
  }, []);

  const showTip = useCallback((el: HTMLElement) => {
    const t = tipRef.current;
    const text = el.dataset.tip;
    if (!t || !text) return;
    t.textContent = text;
    t.style.display = "block";
    const r = el.getBoundingClientRect();
    const tr = t.getBoundingClientRect();
    // 夹紧基准 = 该控件所在显示器（视口是虚拟桌面并集，跨屏时按视口夹会把提示推到屏外）。
    const mon = uiMonitor({ x: r.left + r.width / 2, y: r.top + r.height / 2, w: 0, h: 0 });
    const mx = mon ? mon.x : 0;
    const mw = mon ? mon.w : window.innerWidth;
    const my = mon ? mon.y : 0;
    let left = r.left + r.width / 2 - tr.width / 2;
    left = Math.max(mx + 4, Math.min(left, mx + mw - tr.width - 4));
    let top = r.top - tr.height - 6;
    if (top < my + 4) top = r.bottom + 6;
    t.style.left = `${left}px`;
    t.style.top = `${top}px`;
  }, [uiMonitor]);
  const onTipOver = useCallback(
    (e: RPointerEvent<HTMLDivElement>) => {
      const el = (e.target as HTMLElement).closest?.("[data-tip]") as HTMLElement | null;
      if (el) showTip(el);
      else hideTip();
    },
    [showTip, hideTip],
  );
  // 只在真正离开「带提示的元素」时隐藏：在按钮内部子节点间移动会触发 pointerout，
  // 用 relatedTarget 判定，避免提示闪烁。
  const onTipOut = useCallback(
    (e: RPointerEvent<HTMLDivElement>) => {
      const from = (e.target as HTMLElement).closest?.("[data-tip]");
      const to = (e.relatedTarget as HTMLElement | null)?.closest?.("[data-tip]");
      if (!from || from !== to) hideTip();
    },
    [hideTip],
  );

  const onPointerDown = useCallback(
    (e: RPointerEvent<HTMLDivElement>) => {
      if (busy || recording) return;
      // 右键（用户 2026-09-21 明确要求，PixPin 同款手感）：
      //   未选中 → 取消，直接退出截图；已选中 → 返回，退回自由框选态（不关 overlay）。
      if (e.button === 2) {
        e.preventDefault();
        hideTip();
        if (committed && selection) {
          resetSelection();
        } else {
          void invoke("close_overlay");
        }
        return;
      }
      if (e.button !== 0) return;
      hideTip();
      const px = e.clientX;
      const py = e.clientY;
      (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);

      // 文字编辑中先落笔提交
      if (textEdit) {
        commitText();
        return;
      }

      // 选区已定
      if (selection && committed) {
        const inSel =
          px >= selection.x &&
          px <= selection.x + selection.w &&
          py >= selection.y &&
          py <= selection.y + selection.h;
        // 取色器：点击处取背景像素色 → 写剪贴板 + 设为当前色（选区内外均可）
        if (tool === "eyedropper") {
          const c = sampleColor(px, py);
          if (c) {
            setColor(c);
            void navigator.clipboard.writeText(c).catch(() => {});
          }
          return;
        }
        // 马赛克：选区内按拖拽轨迹像素化背景
        if (tool === "mosaic" && inSel) {
          gestureRef.current = { mode: "draw" };
          setDraft({ kind: "mosaic", points: [{ x: px, y: py }], width: width * 6 });
          return;
        }
        // 墨迹工具且落在选区内 → 开始绘制
        if (tool && INK_TOOLS.includes(tool) && inSel) {
          if (tool === "text") {
            setTextEdit({ x: px, y: py });
            setTextValue("");
            return;
          }
          if (tool === "number") {
            setShapes((prev) => [
              ...prev,
              { kind: "number", x: px, y: py, n: numRef.current++, color, width },
            ]);
            setRedo([]);
            return;
          }
          gestureRef.current = { mode: "draw" };
          if (tool === "rect") setDraft({ kind: "rect", x: px, y: py, w: 0, h: 0, color, width });
          else if (tool === "ellipse")
            setDraft({ kind: "ellipse", x: px, y: py, w: 0, h: 0, color, width });
          else if (tool === "arrow")
            setDraft({ kind: "arrow", x1: px, y1: py, x2: px, y2: py, color, width });
          else if (tool === "pencil")
            setDraft({ kind: "pencil", points: [{ x: px, y: py }], color, width });
          return;
        }

        // 无墨迹工具（编辑态）：命中控制点 → 缩放；选区内 → 移动
        const h = hitHandle(selection, px, py);
        if (h) {
          gestureRef.current = { mode: "resize", handle: h, sx: px, sy: py, orig: selection };
          setGridOn(true);
          return;
        }
        if (
          px >= selection.x &&
          px <= selection.x + selection.w &&
          py >= selection.y &&
          py <= selection.y + selection.h
        ) {
          gestureRef.current = { mode: "move", sx: px, sy: py, orig: selection };
          setGridOn(true);
          return;
        }
        // 选区外落点 → 重开新选区（清空标注）
      }

      // 新建选区
      pendingWinRef.current = hoverWinRef.current; // 记录按下时命中的窗口，供「点击选窗」
      hoverWinRef.current = null;
      hoverLabelRef.current = null;
      // 稳定窗挂起候选与指纹同批清掉：否则按下后 140ms dwell 到期，applyHi
      // 把旧候选提交回来，识别框又盖回选区上（双框的第二条来路）。
      hoverPendingRef.current = null;
      hoverKeyRef.current = null;
      setShapes([]);
      setRedo([]);
      setDraft(null);
      setTool(null);
      setCommitted(false);
      numRef.current = 1;
      gestureRef.current = { mode: "new", sx: px, sy: py };
      setGridOn(true);
      setSelection({ x: px, y: py, w: 0, h: 0 });
    },
    [busy, recording, textEdit, commitText, selection, committed, tool, color, width, sampleColor, resetSelection],
  );

  const onPointerMove = useCallback(
    (e: RPointerEvent<HTMLDivElement>) => {
      const px = e.clientX;
      const py = e.clientY;
      const g = gestureRef.current;

      // 放大镜：只写 cursorRef（on=是否显示），实际绘制由 rAF 循环做——move 里零 setState。
      cursorRef.current = {
        x: px,
        y: py,
        on: !recording && (!committed || (tool === "eyedropper" && !g)),
      };
      // 取色器悬停时不做窗口检测，直接结束（放大镜已给出取样反馈）。
      if (!g && committed && tool === "eyedropper") return;
      // 智能识别：未提交、无手势、无工具时命中光标下的 UI 元素链（UIA）或顶层窗口。
      if (!g && !committed && tool === null && settingsRef.current.capture_ui_detect) {
        const { x: sx, y: sy } = scaleRef.current;
        const b = boundsRef.current;
        const gx = px * sx + b.x; // 光标物理坐标
        const gy = py * sy + b.y;
        lastPointerRef.current = { x: gx, y: gy };
        const now = performance.now();
        // 节流：常驻 UIA 线程单次往返 ~1-3ms，60ms 已足够跟手；在途时不重复发。
        if (now - uiaLastRef.current >= 60 && !uiaInflightRef.current) {
          uiaLastRef.current = now;
          uiaInflightRef.current = true;
          invoke<ElementInfo[]>("ui_chain_at", { x: Math.round(gx), y: Math.round(gy) })
            .then((chain) => {
              const list = chain ?? [];
              // 最内层元素换了才把层级归零——同一元素内移动要保住用户滚轮选的层级。
              const key = list.length
                ? `${list[0].x},${list[0].y},${list[0].w},${list[0].h}`
                : null;
              if (key !== uiaKeyRef.current) {
                uiaKeyRef.current = key;
                uiaLevelRef.current = 0;
              }
              uiaChainRef.current = list;
              resolveHover(gx, gy, true);
            })
            .catch(() => {
              uiaChainRef.current = [];
              uiaKeyRef.current = null;
            })
            .finally(() => {
              uiaInflightRef.current = false;
            });
        }
        // 过渡帧的即时反馈：光标仍在上次命中的最内层元素里 → 沿用当前层级，
        // 否则退回顶层窗口命中（新链回来前不闪断，也不在旧元素上拖影）。
        const c0 = uiaChainRef.current[0];
        const inChain =
          c0 != null && gx >= c0.x && gx <= c0.x + c0.w && gy >= c0.y && gy <= c0.y + c0.h;
        resolveHover(gx, gy, inChain);
        return;
      }
      if (!g) return;
      if (g.mode === "new") {
        setSelection(normRect(g.sx, g.sy, px, py));
      } else if (g.mode === "move") {
        const dx = px - g.sx;
        const dy = py - g.sy;
        let nx = g.orig.x + dx;
        let ny = g.orig.y + dy;
        nx = Math.max(0, Math.min(nx, window.innerWidth - g.orig.w));
        ny = Math.max(0, Math.min(ny, window.innerHeight - g.orig.h));
        setSelection({ x: nx, y: ny, w: g.orig.w, h: g.orig.h });
      } else if (g.mode === "resize") {
        setSelection(resizeRect(g.orig, g.handle, px - g.sx, py - g.sy));
      } else if (g.mode === "draw") {
        setDraft((d) => {
          if (!d) return d;
          if (d.kind === "rect" || d.kind === "ellipse")
            return { ...d, w: px - d.x, h: py - d.y };
          if (d.kind === "arrow") return { ...d, x2: px, y2: py };
          if (d.kind === "pencil") return { ...d, points: [...d.points, { x: px, y: py }] };
          if (d.kind === "mosaic") return { ...d, points: [...d.points, { x: px, y: py }] };
          return d;
        });
      }
    },
    [recording, committed, tool, windows, sampleColor, resolveHover],
  );

  const onPointerUp = useCallback(
    (e: RPointerEvent<HTMLDivElement>) => {
      const g = gestureRef.current;
      gestureRef.current = null;
      setGridOn(false);
      try {
        (e.currentTarget as HTMLElement).releasePointerCapture(e.pointerId);
      } catch {
        /* ignore */
      }
      if (!g) return;
      if (g.mode === "new") {
        const pending = pendingWinRef.current;
        pendingWinRef.current = null;
        setSelection((sel) => {
          if (!sel) return sel;
          const n = normalize(sel);
          if (n.w < MIN_SEL || n.h < MIN_SEL) {
            // 拖拽过小视作点击：若按下时命中窗口，直接选窗并 commit；否则取消
            if (pending) {
              setCommitted(true);
              return pending;
            }
            return null;
          }
          setCommitted(true);
          return n;
        });
      } else if (g.mode === "move" || g.mode === "resize") {
        setSelection((sel) => (sel ? normalize(sel) : sel));
      } else if (g.mode === "draw") {
        setDraft((d) => {
          if (d) {
            // 丢弃零尺寸误触
            const empty =
              (d.kind === "rect" || d.kind === "ellipse") &&
              Math.abs(d.w) < 2 &&
              Math.abs(d.h) < 2;
            const emptyArrow =
              d.kind === "arrow" && Math.abs(d.x2 - d.x1) < 2 && Math.abs(d.y2 - d.y1) < 2;
            if (!empty && !emptyArrow) {
              const norm =
                d.kind === "rect" || d.kind === "ellipse"
                  ? { ...d, ...normalize({ x: d.x, y: d.y, w: d.w, h: d.h }) }
                  : d;
              setShapes((prev) => [...prev, norm]);
              setRedo([]);
            }
          }
          return null;
        });
      }
    },
    [],
  );

  // 滚轮：在识别层级链上放大/缩小（下滚 → 父容器，上滚 → 更内层的元素）。
  // 只在「未提交 + 元素模式」生效；其余阶段滚轮不做事（避免与后续缩放交互抢）。
  const onWheel = useCallback(
    (e: RWheelEvent<HTMLDivElement>) => {
      if (committed || recording || tool !== null) return;
      if (recogModeRef.current !== "element") return;
      const chain = uiaChainRef.current;
      if (chain.length < 2) return;
      const next = Math.max(
        0,
        Math.min(chain.length - 1, uiaLevelRef.current + (e.deltaY > 0 ? 1 : -1)),
      );
      if (next === uiaLevelRef.current) return;
      uiaLevelRef.current = next;
      const last = lastPointerRef.current;
      if (last) resolveHover(last.x, last.y, true, true);
    },
    [committed, recording, tool, resolveHover],
  );

  const canUndo = shapes.length > 0;
  const canRedo = redo.length > 0;

  const undo = useCallback(() => {
    setShapes((prev) => {
      if (prev.length === 0) return prev;
      const last = prev[prev.length - 1];
      setRedo((r) => [...r, last]);
      return prev.slice(0, -1);
    });
  }, []);
  const doRedo = useCallback(() => {
    setRedo((prev) => {
      if (prev.length === 0) return prev;
      const last = prev[prev.length - 1];
      setShapes((s) => [...s, last]);
      return prev.slice(0, -1);
    });
  }, []);

  const selectTool = useCallback((t: Tool) => {
    setTextEdit((cur) => (cur ? null : cur));
    setTool((cur) => (cur === t ? null : t));
  }, []);

  // ---- 工具条拖动（PixPin 同款 ⋮⋮ 手柄）----
  // null = 跟随选区的默认位（选区下方 10px、越界翻转）；有值 = 用户拖到哪儿算哪儿，
  // 直到本次 overlay 会话结束（`overlay-shown` 重置）。拖动只改位置，不动任何标注状态。
  const [barPos, setBarPos] = useState<{ left: number; top: number } | null>(null);
  const barRef = useRef<HTMLDivElement | null>(null);
  const barDragRef = useRef<{ px: number; py: number; ox: number; oy: number; w: number; h: number } | null>(null);

  // ---- 工具条 & Popover 定位（选区正下方 ~10px，越界翻转）----
  const barBox = useMemo(() => {
    if (!selection) return null;
    // 两个常量必须跟着 theme.css 的 --btn / --bar-h 走（2026-09-21 放大一档后同步）：
    // BAR_W_EST 实测 847 = 20 个 32px 按钮(640) + 4 条分隔线含外边距(28) + 两个下拉(72+43)
    //   + 工具条左右内边距(12) + 上下左右边框(2) + 25 个 2px 间距(50)；
    //   取 860 留余量——它只用于居中与越界夹紧，**低估**会让工具条贴近屏幕右缘时被窗口裁掉。
    // BAR_H 取 50 而非 48：工具条没设 box-sizing: border-box，实际渲染高 = --bar-h + 2px 边框，
    //   写 48 会让「贴底翻转」判断和 Popover 间距各差 2px。
    const BAR_W_EST = 860;
    const BAR_H = 50;
    const GAP = 10;
    // 夹紧基准 = 选区所在显示器的物理矩形（找不到才退回视口）：overlay 视口是整个虚拟
    // 桌面并集，按视口夹紧会把条子推到物理屏外 → 按钮点不到（2026-10-15 用户报障）。
    const mon = uiMonitor(selection) ?? { x: 0, y: 0, w: window.innerWidth, h: window.innerHeight };
    const cx = selection.x + selection.w / 2;
    let left = cx - BAR_W_EST / 2;
    left = Math.max(mon.x + 8, Math.min(left, mon.x + mon.w - BAR_W_EST - 8));
    let top = selection.y + selection.h + GAP;
    let below = true;
    if (top + BAR_H > mon.y + mon.h - 8) {
      top = selection.y - BAR_H - GAP;
      below = false;
      if (top < mon.y + 8) {
        top = selection.y + selection.h - BAR_H - GAP; // 塞进选区内底部
        below = true;
      }
    }
    return { left, top, below, barH: BAR_H, gap: GAP };
  }, [selection, uiMonitor]);

  // 保存条/预览卡默认 CSS 是 `left:50%`（相对**视口**居中）。overlay 视口是整个虚拟桌面
  // 并集 → 多屏时条子会落在并集中心（可能不在任何物理屏内，用户看不到按钮）。
  // 改成按选区所在显示器居中：left 显式给像素、translateX(-50%) 仍由 CSS 保留。
  // 同时把 `max-width: calc(100vw - 32px)`（100vw = 并集宽）压到显示器宽，否则长路径
  // 会把条子撑得比物理屏还宽。
  const saveBarCenter = useMemo(() => {
    const mon = selection ? uiMonitor(selection) : null;
    if (!mon) return undefined;
    return {
      left: `${mon.x + mon.w / 2}px`,
      maxWidth: `${Math.max(240, mon.w - 32)}px`,
    };
  }, [selection, uiMonitor]);

  const barLeft = barPos?.left ?? barBox?.left ?? 0;
  const barTop = barPos?.top ?? barBox?.top ?? 0;
  // 拖动起点的「当前条位置」：每帧同步，避免闭包读到旧值。
  const barCurRef = useRef({ left: barLeft, top: barTop });
  barCurRef.current = { left: barLeft, top: barTop };

  const onBarDragStart = useCallback((e: React.PointerEvent<HTMLButtonElement>) => {
    e.stopPropagation(); // 不能落到根节点的手势/框选逻辑里
    const el = barRef.current;
    if (!el) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    barDragRef.current = {
      px: e.clientX,
      py: e.clientY,
      ox: barCurRef.current.left,
      oy: barCurRef.current.top,
      w: el.offsetWidth,
      h: el.offsetHeight,
    };
  }, []);

  const onBarDragMove = useCallback((e: React.PointerEvent<HTMLButtonElement>) => {
    const d = barDragRef.current;
    if (!d) return;
    // 夹在窗内：整条永远可见可点（拖出屏幕就再也拉不回来，索性不让它出窗）
    setBarPos({
      left: Math.max(4, Math.min(d.ox + (e.clientX - d.px), window.innerWidth - d.w - 4)),
      top: Math.max(4, Math.min(d.oy + (e.clientY - d.py), window.innerHeight - d.h - 4)),
    });
  }, []);

  const onBarDragEnd = useCallback(() => {
    barDragRef.current = null;
  }, []);

  const sizeLabel = useMemo(() => {
    if (!selection) return null;
    const { x: sx, y: sy } = scaleRef.current;
    const pw = Math.round(selection.w * sx);
    const ph = Math.round(selection.h * sy);
    // 文案格式照 PixPin（docs/UI设计规范.md §1.5 与实机图读数一致）：`x,y  W x H px`，
    // 如 `683,229 934 x 488 px`。坐标取选区左上角的**物理像素**（与产物裁剪原点同源，
    // 即 selOrigin），这样用户报坐标时和截图产物对得上。
    const o = selOrigin();
    let top = selection.y - 26;
    if (top < 2) top = selection.y + 4;
    return { left: Math.max(2, selection.x), top, text: `${o.x},${o.y}  ${pw} x ${ph} px` };
  }, [selection, selOrigin]);

  const showPopover = committed && tool !== null && INK_TOOLS.includes(tool);

  const rootStyle: CSSProperties = {
    cursor: committed && tool ? "crosshair" : committed ? "default" : "crosshair",
  };

  const textStyle: CSSProperties | undefined = textEdit
    ? {
        left: textEdit.x,
        top: textEdit.y,
        color,
        fontSize: 14 + width * 3,
        caretColor: color,
      }
    : undefined;

  // 暂存件（.pending 下）还没进产物目录，显示全路径会让用户以为文件已经躺在那；
  // 这种只报文件名，录制产物（路径已是最终路径）照旧显示全路径。
  const pendingName = savePrompt ? pendingFileName(savePrompt.path) : null;

  return (
    <div
      className="overlay-root"
      style={rootStyle}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerOver={onTipOver}
      onPointerOut={onTipOut}
      onWheel={onWheel}
      // 右键在 overlay 里是「取消/返回」（见 onPointerDown），必须吃掉 WebView2 的
      // 原生上下文菜单，否则每次右键都会弹系统菜单盖住界面。
      onContextMenu={(e) => e.preventDefault()}
      onDoubleClick={(e) => {
        if (
          committed &&
          tool === null &&
          selection &&
          e.clientX >= selection.x &&
          e.clientX <= selection.x + selection.w &&
          e.clientY >= selection.y &&
          e.clientY <= selection.y + selection.h
        ) {
          void finish(settingsRef.current.capture_auto_save, settingsRef.current.capture_copy_after);
        }
      }}
    >
      {dataUrl && (
        <img
          ref={imgRef}
          className="overlay-bg"
          src={dataUrl}
          onLoad={onBgLoad}
          alt=""
          draggable={false}
          style={recording ? { display: "none" } : undefined}
        />
      )}

      {/* 未开始框选：整屏暗遮罩 */}
      {ready && !recording && !selection && <div className="overlay-dim" />}

      {/* 智能窗口检测高亮：命令式 DOM，rAF 循环直改 style（不走 state）*/}
      <div
        ref={hiElRef}
        className="overlay-winhi"
        style={{ display: "none" }}
      />

      {/* 智能识别标签：元素类型 / 层级 / 名称，rAF 循环直改文本与位置 */}
      <div ref={recogElRef} className="overlay-recog" style={{ display: "none" }} />

      {/* 选区（box-shadow 挖空）；录制态改用无遮罩纯边框，露出实时桌面 */}
      {selection && !recording && (
        <div
          className="overlay-sel"
          style={{ left: selection.x, top: selection.y, width: selection.w, height: selection.h }}
        >
          {/* 取景/移动/缩放时的三分辅助线 */}
          {gridOn && (
            <div className="overlay-grid" aria-hidden>
              <span className="v1" />
              <span className="v2" />
              <span className="h1" />
              <span className="h2" />
            </div>
          )}
        </div>
      )}

      {/* 录制态选区边框（红框，无暗遮罩，露出实时桌面供抓录）。
          A+（2026-09-24）：坐标外扩 2px——整圈红压在 crop（=选区）之外，成品零红。
          必须用坐标外扩而不是 box-sizing/outline：outline 不进 getBoundingClientRect
          会把 rec_input_passthrough 的红框环算成空（录制期红框被 region 裁没）；
          content-box 也不改 `left` 参照的是 border box 这一事实。外扩后 rect=选区±2、
          border=2 → push() 的 rect+inset 挖洞环与可视红带自洽，后端零改动。 */}
      {selection && recording && (
        <div
          className="overlay-recframe"
          style={{
            left: selection.x - 2,
            top: selection.y - 2,
            width: selection.w + 4,
            height: selection.h + 4,
          }}
        />
      )}

      {/* 开录倒计时（record_countdown>0）：整屏拦截层 + 选区中央的大号秒数。
          拦截层同时是取消手势——倒计时里点任意处/Esc 都取消，避免用户在倒计时中
          点到工具条做出「截图/贴图」这类与即将开录冲突的动作。 */}
      {countdown && (
        <div
          className="overlay-countdown"
          onPointerDown={(e) => {
            e.stopPropagation();
            setCountdown(null);
          }}
          onWheel={(e) => e.stopPropagation()}
          // 双击也要吃掉：根节点的 onDoubleClick 是「保存并复制」，倒计时里双击选区
          // 会先把截图落盘、随后倒计时归零又开录（凭空多存一份用户没要的图）。
          onDoubleClick={(e) => e.stopPropagation()}
        >
          {selection && (
            <div
              className="overlay-countdown-num"
              style={{
                left: selection.x + selection.w / 2,
                top: selection.y + selection.h / 2,
              }}
            >
              {countdown.secs}
            </div>
          )}
          <div className="overlay-countdown-tip">
            {countdown.mode === "gif" ? "GIF" : "视频"}录制将在 {countdown.secs} 秒后开始 ·
            点击任意处或按 Esc 取消
          </div>
        </div>
      )}

      {/* 标注画布 */}
      <canvas ref={canvasRef} className="overlay-canvas" />

      {/* 尺寸标签（设置里可关；A+：录制期隐藏——贴顶 fallback top=y+4 会进 crop） */}
      {settings.capture_show_size_tag && selection && sizeLabel && !recording && (
        <div className="overlay-size" style={{ left: sizeLabel.left, top: sizeLabel.top }}>
          {sizeLabel.text}
        </div>
      )}

      {/* 八向控制点（编辑态显示）*/}
      {selection &&
        committed &&
        !recording &&
        tool === null &&
        handlePoints(selection).map((p) => (
          <div
            key={p.id}
            className={`overlay-handle ${p.id}`}
            style={{ left: p.cx, top: p.cy }}
            onPointerDown={(e) => {
              e.stopPropagation();
              (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
              gestureRef.current = { mode: "resize", handle: p.id, sx: e.clientX, sy: e.clientY, orig: selection };
            }}
            onPointerMove={onPointerMove}
            onPointerUp={onPointerUp}
          />
        ))}

      {/* 文字标注输入框 */}
      {textEdit && (
        <textarea
          className="overlay-textedit"
          style={textStyle}
          autoFocus
          value={textValue}
          onChange={(e) => setTextValue(e.target.value)}
          onPointerDown={(e) => e.stopPropagation()}
          onBlur={commitText}
          onKeyDown={(e) => {
            e.stopPropagation();
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              commitText();
            } else if (e.key === "Escape") {
              e.preventDefault();
              setTextEdit(null);
              setTextValue("");
            }
          }}
        />
      )}

      {/* 二级属性 Popover（颜色 + 粗细）*/}
      {showPopover && barBox && (
        <div
          className="overlay-popover"
          style={{
            left: barLeft,
            // 拖动过就固定挂在条下方（上下翻转的语义在自由位置下没有意义）
            top: barPos
              ? barTop + barBox.barH + 6
              : barBox.below
                ? barBox.top + barBox.barH + 6
                : barBox.top - 40,
          }}
          onPointerDown={(e) => e.stopPropagation()}
        >
          <div className="overlay-swatches">
            {PALETTE.map((c) => (
              <button
                key={c}
                type="button"
                className={`overlay-swatch ${c === color ? "active" : ""}`}
                style={{ background: c }}
                onClick={() => setColor(c)}
                data-tip={c}
                aria-label={`颜色 ${c}`}
              />
            ))}
          </div>
          <div className="overlay-widths">
            {WIDTHS.map((w, i) => (
              <button
                key={w}
                type="button"
                className={`overlay-width ${w === width ? "active" : ""}`}
                onClick={() => setWidth(w)}
                data-tip={["细", "中", "粗"][i]}
                aria-label={`粗细 ${["细", "中", "粗"][i]}`}
              >
                <i style={{ width: 4 + i * 4, height: 4 + i * 4 }} />
              </button>
            ))}
          </div>
        </div>
      )}

      {/* 标注工具条 */}
      {committed && selection && barBox && !recording && (
        <div
          ref={barRef}
          className="overlay-toolbar"
          style={{ left: barLeft, top: barTop }}
          onPointerDown={(e) => e.stopPropagation()}
        >
          {/* 拖动手柄（PixPin 同款 ⋮⋮）：整条工具条可拖离选区，不再挡住选区内容 */}
          <button
            className="overlay-tool overlay-bargrip"
            data-tip="拖动工具条"
            aria-label="拖动工具条"
            onPointerDown={onBarDragStart}
            onPointerMove={onBarDragMove}
            onPointerUp={onBarDragEnd}
            onPointerCancel={onBarDragEnd}
          >
            <Icon name="grip" />
          </button>
          <span className="overlay-divider" />
          <button
            className="overlay-tool"
            data-tip="撤销 (Ctrl+Z)"
            aria-label="撤销"
            disabled={!canUndo}
            onClick={undo}
          >
            <Icon name="undo" />
          </button>
          <button
            className="overlay-tool"
            data-tip="重做 (Ctrl+Y)"
            aria-label="重做"
            disabled={!canRedo}
            onClick={doRedo}
          >
            <Icon name="redo" />
          </button>
          <span className="overlay-divider" />

          {(
            [
              ["rect", "矩形"],
              ["ellipse", "椭圆"],
              ["arrow", "箭头"],
              ["pencil", "铅笔"],
              ["text", "文字"],
              ["number", "序号"],
              ["mosaic", "马赛克"],
              ["eyedropper", "取色"],
            ] as [Tool, string][]
          ).map(([t, label]) => (
            <button
              key={t}
              className={`overlay-tool ${tool === t ? "active" : ""}`}
              data-tip={label}
              aria-label={label}
              onClick={() => selectTool(t)}
            >
              <Icon name={t} />
            </button>
          ))}

          <span className="overlay-divider" />
          <button
            className="overlay-tool"
            data-tip="贴图（钉在屏上）"
            aria-label="贴图"
            disabled={busy}
            onClick={() => void doPin()}
          >
            <Icon name="pin" />
          </button>
          <button
            className="overlay-tool"
            data-tip="OCR 文字识别"
            aria-label="文字识别"
            disabled={busy}
            onClick={() => void doOcr()}
          >
            <Icon name="ocr" />
          </button>
          <button
            className="overlay-tool"
            data-tip="长截图（滚动拼接）"
            aria-label="长截图"
            disabled={busy}
            onClick={() => void doLong()}
          >
            <Icon name="long" />
          </button>
          <span className="overlay-divider" />
          {/* 录制模式：视频 / GIF（切换后帧率下拉选项随之变化）*/}
          <button
            className={`overlay-tool ${recMode === "video" ? "active" : ""}`}
            data-tip="视频模式"
            aria-label="视频模式"
            onClick={() => changeRecMode("video")}
          >
            <Icon name="video" />
          </button>
          <button
            className={`overlay-tool ${recMode === "gif" ? "active" : ""}`}
            data-tip="GIF 模式"
            aria-label="GIF 模式"
            onClick={() => changeRecMode("gif")}
          >
            <Icon name="gif" />
          </button>
          {/* 帧率下拉：视频 15/24/30/60，GIF 10/15/24 */}
          <select
            className="overlay-recsel"
            data-tip="帧率"
            aria-label="帧率"
            value={recFps}
            onChange={(e) => setRecFps(Number(e.target.value))}
          >
            {(recMode === "gif" ? REC_FPS_GIF : REC_FPS_VIDEO).map((f) => (
              <option key={f} value={f}>
                {f} FPS
              </option>
            ))}
          </select>
          {/* 画质下拉：低/中/高 → 码率档位键 2000/8000/20000（与设置页同一组预设） */}
          <select
            className="overlay-recsel"
            data-tip="画质（码率档位）"
            aria-label="画质"
            value={recBitrate}
            onChange={(e) => setRecBitrate(Number(e.target.value))}
          >
            {QUALITY_PRESETS.map((q) => (
              <option key={q.k} value={q.k}>
                {q.label}
              </option>
            ))}
          </select>
          {/* 录音频开关（D7）：录屏入口以前没有音频选项，只能去设置页翻。
              区域/全屏录制走 dshow 设备（设备在设置页选）；窗口录制抓系统声音。 */}
          <button
            type="button"
            className="overlay-recsel"
            data-tip={
              recAudio
                ? "录系统声音：开（区域/全屏走设置里选的音频设备；窗口录制抓系统声音）"
                : "录系统声音：关（点一下打开）"
            }
            aria-label="录音频"
            aria-pressed={recAudio}
            disabled={busy}
            onClick={() => void toggleRecAudio()}
          >
            {recAudio ? "有声音" : "静音"}
          </button>
          {/* 开始录制（按当前模式/帧率/画质；record_countdown>0 时先倒计时）*/}
          <button
            className="overlay-tool done"
            data-tip={recMode === "gif" ? "开始录制 GIF" : "开始录制视频"}
            aria-label="开始录制"
            disabled={busy}
            onClick={() => beginRecord()}
          >
            <Icon name={recMode === "gif" ? "gif" : "video"} />
          </button>

          <span className="overlay-divider" />
          <button
            className="overlay-tool"
            data-tip="保存 (Ctrl+S)"
            aria-label="保存"
            disabled={busy}
            onClick={() => void finish(true, false)}
          >
            <Icon name="save" />
          </button>
          <button
            className="overlay-tool"
            data-tip="复制 (Ctrl+C)"
            aria-label="复制"
            disabled={busy}
            onClick={() => void finish(false, true)}
          >
            <Icon name="copy" />
          </button>
          <button
            className="overlay-tool"
            data-tip="关闭 (Esc)"
            aria-label="关闭"
            onClick={() => invoke("close_overlay")}
          >
            <Icon name="close" />
          </button>
          <button
            className="overlay-tool done"
            data-tip="完成：保存并复制 (Enter)"
            aria-label="完成"
            disabled={busy}
            onClick={() => void finish(true, true)}
          >
            <Icon name="check" />
          </button>
        </div>
      )}

      {/* 悬浮提示（自绘 tooltip，见 showTip 注释）：命令式 DOM，z-index 最高 */}
      <div ref={tipRef} className="overlay-tip" style={{ display: "none" }} />

      {/* 放大镜：命令式 DOM，rAF 循环直绘（不走 state）*/}
      <div ref={loupeRef} className="overlay-loupe" style={{ display: "none" }}>
        <canvas ref={loupeCanvasRef} width={LOUPE_VIEW} height={LOUPE_VIEW} className="overlay-loupe-canvas" />
        <div className="overlay-loupe-info">
          <span className="overlay-loupe-pos" />
          <span className="overlay-loupe-hex">
            <i />
            <b className="overlay-loupe-hexval">—</b>
          </span>
        </div>
      </div>

      {/* 录制控制条（选区上方居中，越界翻到下方）*/}
      {recording && selection && (
        <div
          ref={recbarRef}
          className="overlay-recbar"
          style={{
            // A+ 定位：四段决策（上/下/横向逃逸/极端退回）见 recbarPos 注释——
            // 条子保证不落进 crop，成品里没有录制条。
            ...recbarPos(selection, window.innerWidth, window.innerHeight, uiMonitor(selection)),
          }}
          onPointerDown={(e) => e.stopPropagation()}
        >
          <span
            className={
              recording.pausedAt !== null || stopping ? "overlay-recdot paused" : "overlay-recdot"
            }
          />
          <span className="overlay-rectime">
            {stopping ? (
              <>收尾中{transcode !== null ? ` · 转码 ${transcode}%` : "…"}</>
            ) : transcode !== null ? (
              <>GIF 转码中 {transcode}%</>
            ) : (
              <>
                {recording.mode === "gif" ? "GIF" : "REC"} {fmtElapsed(recElapsed)} ·{" "}
                {recording.mode === "gif" ? Math.min(recFps, 24) : recFps}f
                {recording.pausedAt !== null ? " · 已暂停" : ""}
              </>
            )}
          </span>
          <button
            type="button"
            className="overlay-reccancel"
            data-tip="取消录制（丢弃不保存）"
            aria-label="取消录制"
            disabled={stopping}
            onClick={() => void discardRecord()}
          >
            ×
          </button>
          {/* 暂停/继续：GIF 模式同样支持——分段拼接对两种格式一致。
              tooltip 不写「(空格)」：录制中键盘已让渡给用户程序，页面收不到空格。 */}
          <button
            type="button"
            className="overlay-recpause"
            data-tip={
              (recording.pausedAt !== null ? "继续录制" : "暂停录制") +
              (pauseKey ? `（${pauseKey}）` : "")
            }
            aria-label={recording.pausedAt !== null ? "继续录制" : "暂停录制"}
            disabled={transcode !== null || stopping}
            onClick={() => void togglePauseRecord()}
          >
            <Icon name={recording.pausedAt !== null ? "play" : "pause"} />
            {recording.pausedAt !== null ? "继续" : "暂停"}
          </button>
          <button
            type="button"
            className="overlay-recstop"
            data-tip={`停止录制（${captureKey}）`}
            aria-label="停止录制"
            disabled={stopping}
            onClick={() => void stopRecord()}
          >
            <Icon name="stop" />
            {stopping ? "收尾中" : "停止"}
          </button>
        </div>
      )}

      {/* 回放预览卡（保存确认阶段，mp4/gif）：压在保存条上方。mp4 用原生
          <video controls>——播放/暂停、进度拖动、时间、音量、倍速、全屏一次拿齐
          （用户 2026-09-24：「不能只做一个开始和暂停键」）；GIF 用 <img> 循环预览
          （GIF 本身即动画，无需控件）。src 走 asset:// 协议（convertFileSrc），
          产物目录已由 allow_asset_captures 进白名单。指针/滚轮/双击都在卡内截断：
          根节点 onDoubleClick=保存并复制，双击视频全屏不能误触它。 */}
      {savePrompt && (savePrompt.kind === "mp4" || savePrompt.kind === "gif") && (
        <div
          className="overlay-savepreview"
          style={saveBarCenter}
          onPointerDown={(e) => e.stopPropagation()}
          onDoubleClick={(e) => e.stopPropagation()}
          onWheel={(e) => e.stopPropagation()}
        >
          {savePrompt.kind === "mp4" ? (
            <video
              ref={videoRef}
              className="overlay-savepreview-media"
              src={convertFileSrc(savePrompt.path)}
              controls
              preload="metadata"
              playsInline
            />
          ) : (
            <img
              className="overlay-savepreview-media"
              src={convertFileSrc(savePrompt.path)}
              alt="GIF 回放预览"
              draggable={false}
            />
          )}
          <div className="overlay-savepreview-foot">
            {savePrompt.kind === "mp4" && (
              <button
                type="button"
                className="overlay-savebar-btn"
                onClick={() => {
                  const v = videoRef.current;
                  if (!v) return;
                  v.currentTime = 0;
                  void v.play();
                }}
              >
                重播
              </button>
            )}
            <span className="overlay-savepreview-hint">
              {savePrompt.kind === "mp4"
                ? "空格 播放/暂停 · 控制条可拖进度 / 调音量 / 倍速 / 全屏"
                : "GIF 循环播放中"}
            </span>
          </div>
        </div>
      )}

      {/* 保存确认条（capture_confirm_save 开启时）：快速保存 / 另存为… / 删除 */}
      {savePrompt && (
        <div className="overlay-savebar" style={saveBarCenter} onPointerDown={(e) => e.stopPropagation()}>
          <span className="overlay-savebar-path" title={savePrompt.path}>
            {savePrompt.kind === "png"
              ? "截图"
              : savePrompt.kind === "jpg"
                ? "截图（JPG）"
                : savePrompt.kind === "gif"
                  ? "GIF"
                  : "录屏"}
            {pendingName ? `将保存到默认目录：${pendingName}` : `已保存到 ${savePrompt.path}`}
          </span>
          <button type="button" className="overlay-savebar-btn" onClick={() => void confirmKeep()}>
            快速保存
          </button>
          <button
            type="button"
            className="overlay-savebar-btn"
            disabled={busy}
            onClick={() => void copyProduct()}
          >
            {copiedFlash ? "已复制" : "复制"}
          </button>
          <button
            type="button"
            className="overlay-savebar-btn"
            disabled={busy}
            onClick={() => void confirmSaveAs()}
          >
            另存为…
          </button>
          <button
            type="button"
            className="overlay-savebar-btn overlay-savebar-danger"
            onClick={() => void confirmDiscard()}
          >
            删除
          </button>
        </div>
      )}

      {/* OCR 结果浮层（可关闭）*/}
      {/* 左下角快捷键提示卡：只列真实可用的键（见 hintRows 注释）*/}
      {!busy && hintRows.length > 0 && (
        <div className="overlay-hints">
          {hintRows.map(([k, a]) => (
            <div className="overlay-hint-row" key={k + a}>
              <kbd>{k}</kbd>
              <span>{a}</span>
            </div>
          ))}
        </div>
      )}

      {ocrText !== null && (
        <div className="overlay-ocr" onPointerDown={(e) => e.stopPropagation()}>
          <div className="overlay-ocr-head">
            <span>OCR 结果（{ocrCopied ? "已复制到剪贴板" : "未自动复制到剪贴板"}）</span>
            <button
              type="button"
              className="overlay-ocr-close"
              data-tip="关闭"
              aria-label="关闭"
              onClick={() => setOcrText(null)}
            >
              ×
            </button>
          </div>
          <pre className="overlay-ocr-body">{ocrText}</pre>
        </div>
      )}

      {/* 操作反馈条：保存/复制的成功与失败提示（D12：失败原来只 console.error，
          界面停在原地既不弹条也不给提示）。样式走内联，不动 Overlay.css。 */}
      {flash && (
        <div
          role="status"
          style={{
            position: "fixed",
            left: "50%",
            top: 28,
            transform: "translateX(-50%)",
            zIndex: 9999,
            maxWidth: "72vw",
            padding: "8px 16px",
            borderRadius: 8,
            font: "13px/1.5 system-ui, 'Segoe UI', sans-serif",
            color: flash.kind === "err" ? "#fff" : "#111",
            background: flash.kind === "err" ? "rgba(190, 32, 32, 0.95)" : "rgba(255, 255, 255, 0.95)",
            boxShadow: "0 4px 16px rgba(0, 0, 0, 0.35)",
            pointerEvents: "none",
            whiteSpace: "pre-wrap",
          }}
        >
          {flash.text}
        </div>
      )}
    </div>
  );
}

export default Overlay;
