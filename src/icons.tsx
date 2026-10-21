import { cloneElement, type ReactElement, type SVGProps } from "react";

/* ============================================================
 * SnapPin 共享线性图标集（Lucide 风格，2px 描边）。
 *
 * 为什么独立成模块：覆盖层工具条、悬浮条、剪贴板面板、设置面板都要图标，
 * 各自内联一套会出现第二个图标约定（形状/描边/尺寸各写各的）。
 * 这里唯一一份，Overlay.tsx 已改为 import 本模块。
 *
 * 尺寸：不传 `size` 时不给宽高属性，由 CSS 决定（覆盖层工具条走 `--icon`）；
 * 传 `size` 则写死宽高（面板/列表这类固定尺寸场景）。
 *
 * 新增图标：只加 case，不改既有 path —— 覆盖层那批已按 PixPin 参考图（docs/UI设计规范.md
 * §1.5/§3）逐个人工校准过，改动会让工具条与参考图不符。
 *
 * 状态（2026-09-24）：剪贴板面板与**覆盖层 Overlay.tsx 都已切到本模块**（Overlay 的本地
 * Icon 已删除、改为 import）。抽取前做过等价性证明：23 个图标名逐个渲染出的 DOM 标记与
 * 抽取前的本地定义逐字节相同（`renderToStaticMarkup` 对比，无空标记），未知名字两边都静默。
 * 覆盖层那批 path 已按 PixPin 参考图（docs/UI设计规范.md §1.5/§3）人工校准，**不要再改**。
 * ============================================================ */

/** 图标名（新增图标必须同步加进这个联合类型，避免拼错名字静默渲染空白）。 */
export type IconName =
  // 覆盖层工具条 / 录屏条（PixPin 校准）
  | "grip"
  | "undo"
  | "redo"
  | "rect"
  | "ellipse"
  | "arrow"
  | "pencil"
  | "text"
  | "number"
  | "mosaic"
  | "eyedropper"
  | "pin"
  | "ocr"
  | "long"
  | "save"
  | "copy"
  | "check"
  | "video"
  | "gif"
  | "close"
  | "stop"
  | "pause"
  | "play"
  // 面板/列表
  | "search"
  | "star"
  | "star-filled"
  | "trash"
  | "clipboard"
  | "image"
  | "file"
  // 桌面悬浮条
  | "capture"
  | "settings"
  | "eye-off"
  | "camera"
  // 配置窗左侧导航（16px、1.6 描边；nav- 前缀是为了避免与 capture/pin/save/ocr/clipboard
  // 这些**同名但字形不同**的图标撞车——那几对的 path 各不相同，覆盖会静默换图）
  | "nav-system"
  | "nav-capture"
  | "nav-pin"
  | "nav-clipboard"
  | "nav-save"
  | "nav-annotate"
  | "nav-hotkeys"
  | "nav-builtin"
  | "nav-record"
  | "nav-ocr"
  | "nav-about";

function iconShape(name: string, stroke?: number): ReactElement<SVGProps<SVGSVGElement>> | null {
  const common = {
    viewBox: "0 0 24 24",
    fill: "none",
    stroke: "currentColor",
    // 默认 2（覆盖层/面板那批的口径）；配置窗导航用 1.6 —— 16px 列表里 2 会显得笨。
    // 调用方传了就用调用方的，个别 case（如 camera 的 2.2）仍自带覆盖。
    strokeWidth: stroke ?? 2,
    strokeLinecap: "round" as const,
    strokeLinejoin: "round" as const,
  };
  switch (name) {
    case "grip":
      // 工具条拖动手柄（PixPin 同款 ⋮⋮）：两列三点的抓握点
      return (
        <svg viewBox="0 0 24 24" fill="currentColor" stroke="none">
          {[7, 12, 17].map((y) => (
            <g key={y}>
              <circle cx="9" cy={y} r="1.7" />
              <circle cx="15" cy={y} r="1.7" />
            </g>
          ))}
        </svg>
      );
    case "undo":
      return (
        <svg {...common}>
          <path d="M9 14 4 9l5-5" />
          <path d="M4 9h11a5 5 0 0 1 0 10h-3" />
        </svg>
      );
    case "redo":
      return (
        <svg {...common}>
          <path d="m15 14 5-5-5-5" />
          <path d="M20 9H9a5 5 0 0 0 0 10h3" />
        </svg>
      );
    case "rect":
      return (
        <svg {...common}>
          <rect x="3" y="5" width="18" height="14" rx="1" />
        </svg>
      );
    case "ellipse":
      return (
        <svg {...common}>
          <ellipse cx="12" cy="12" rx="9" ry="7" />
        </svg>
      );
    case "arrow":
      return (
        <svg {...common}>
          <path d="M5 19 19 5" />
          <path d="M11 5h8v8" />
        </svg>
      );
    case "pencil":
      return (
        <svg {...common}>
          <path d="M12 20h9" />
          <path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z" />
        </svg>
      );
    case "text":
      return (
        <svg {...common}>
          <path d="M4 7V5h16v2" />
          <path d="M12 5v14" />
          <path d="M9 19h6" />
        </svg>
      );
    case "number":
      return (
        <svg {...common}>
          <circle cx="12" cy="12" r="9" />
          <path d="M11 8.5 12.8 7.5V16" />
        </svg>
      );
    case "mosaic":
      return (
        <svg {...common}>
          <rect x="3" y="3" width="7" height="7" />
          <rect x="14" y="3" width="7" height="7" />
          <rect x="3" y="14" width="7" height="7" />
          <rect x="14" y="14" width="7" height="7" />
        </svg>
      );
    case "eyedropper":
      return (
        <svg {...common}>
          <path d="m2 22 1-4 11-11 3 3L6 21Z" />
          <path d="m15 6 3-3a2.1 2.1 0 0 1 3 3l-3 3" />
        </svg>
      );
    case "pin":
      return (
        <svg {...common}>
          <path d="M12 17v5" />
          <path d="M9 3h6l-1 4 3 3v2H7v-2l3-3-1-4Z" />
        </svg>
      );
    case "ocr":
      return (
        <svg {...common}>
          <path d="M3 7V5a2 2 0 0 1 2-2h2" />
          <path d="M17 3h2a2 2 0 0 1 2 2v2" />
          <path d="M21 17v2a2 2 0 0 1-2 2h-2" />
          <path d="M7 21H5a2 2 0 0 1-2-2v-2" />
          <path d="M7 9h10M7 13h10M7 17h6" />
        </svg>
      );
    case "long":
      return (
        <svg {...common}>
          <rect x="4" y="3" width="16" height="9" rx="1" />
          <path d="M12 12v9" />
          <path d="m8 17 4 4 4-4" />
        </svg>
      );
    case "save":
      return (
        <svg {...common}>
          <path d="M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2Z" />
          <path d="M17 21v-8H7v8M7 3v5h8" />
        </svg>
      );
    case "copy":
      return (
        <svg {...common}>
          <rect x="9" y="9" width="12" height="12" rx="2" />
          <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
        </svg>
      );
    case "check":
      return (
        <svg {...common}>
          <path d="M20 6 9 17l-5-5" />
        </svg>
      );
    case "video":
      return (
        <svg {...common}>
          <rect x="2" y="6" width="14" height="12" rx="2" />
          <path d="m16 10 6-3v10l-6-3Z" />
        </svg>
      );
    case "gif":
      // 小尺寸（16px 档）下细描边的「GIF」字母画完全糊掉：改粗描边 + 大字母布局，小尺寸可读。
      // 2026-09-21 工具条图标放大到 18px（theme.css --icon）后更清楚，粗描边布局保持不变。
      return (
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.6} strokeLinecap="round" strokeLinejoin="round">
          <rect x="1.6" y="4.6" width="20.8" height="14.8" rx="2.4" />
          <path d="M9.4 10.2H7.6a1.2 1.2 0 0 0-1.2 1.2v1.2a1.2 1.2 0 0 0 1.2 1.2h1.4v-1.2H8.2" />
          <path d="M12.2 10.2v3.6" />
          <path d="M17.6 10.2h-2.2v3.6h2.2M15.4 12h1.6" />
        </svg>
      );
    case "close":
      return (
        <svg {...common}>
          <path d="M18 6 6 18" />
          <path d="m6 6 12 12" />
        </svg>
      );
    case "stop":
      return (
        <svg viewBox="0 0 24 24" fill="currentColor" stroke="none">
          <rect x="6" y="6" width="12" height="12" rx="2" />
        </svg>
      );
    case "pause":
      return (
        <svg viewBox="0 0 24 24" fill="currentColor" stroke="none">
          <rect x="6" y="5" width="4" height="14" rx="1.2" />
          <rect x="14" y="5" width="4" height="14" rx="1.2" />
        </svg>
      );
    case "play":
      return (
        <svg viewBox="0 0 24 24" fill="currentColor" stroke="none">
          <path d="M7 5.2v13.6a1 1 0 0 0 1.5.87l11-6.8a1 1 0 0 0 0-1.74l-11-6.8A1 1 0 0 0 7 5.2Z" />
        </svg>
      );
    // ---- 面板/列表（剪贴板面板等）----
    case "search":
      return (
        <svg {...common}>
          <circle cx="11" cy="11" r="7" />
          <path d="m20 20-3.6-3.6" />
        </svg>
      );
    case "star":
      return (
        <svg {...common}>
          <path d="M12 3.6l2.6 5.3 5.8.85-4.2 4.1 1 5.75L12 16.9l-5.2 2.7 1-5.75-4.2-4.1 5.8-.85Z" />
        </svg>
      );
    case "star-filled":
      return (
        <svg viewBox="0 0 24 24" fill="currentColor" stroke="none">
          <path d="M12 3.6l2.6 5.3 5.8.85-4.2 4.1 1 5.75L12 16.9l-5.2 2.7 1-5.75-4.2-4.1 5.8-.85Z" />
        </svg>
      );
    case "trash":
      return (
        <svg {...common}>
          <path d="M4 7h16" />
          <path d="M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2" />
          <path d="M6 7l1 12a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2l1-12" />
          <path d="M10 11v6M14 11v6" />
        </svg>
      );
    case "clipboard":
      return (
        <svg {...common}>
          <rect x="5" y="4" width="14" height="17" rx="2" />
          <path d="M9 4V3h6v1" />
          <path d="M9 10h6M9 14h4" />
        </svg>
      );
    case "image":
      return (
        <svg {...common}>
          <rect x="3" y="4" width="18" height="16" rx="2" />
          <circle cx="8.5" cy="9.5" r="1.5" />
          <path d="m4 17 4.5-4.5 3.5 3.5 2.5-2.5L20 17" />
        </svg>
      );
    case "file":
      return (
        <svg {...common}>
          <path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8Z" />
          <path d="M14 3v5h5" />
        </svg>
      );
    // ---- 桌面悬浮条（原 Ball.tsx 内联，2026-09-24 并入）----
    case "capture":
      // 取景框：四角括号 + 中线
      return (
        <svg {...common}>
          <path d="M4 8V6a2 2 0 0 1 2-2h2" />
          <path d="M16 4h2a2 2 0 0 1 2 2v2" />
          <path d="M20 16v2a2 2 0 0 1-2 2h-2" />
          <path d="M8 20H6a2 2 0 0 1-2-2v-2" />
          <path d="M8 12h8" />
        </svg>
      );
    case "settings":
      return (
        <svg {...common}>
          <circle cx="12" cy="12" r="3" />
          <path d="M12 2v3M12 19v3M2 12h3M19 12h3M4.9 4.9l2.1 2.1M17 17l2.1 2.1M19.1 4.9 17 7M7 17l-2.1 2.1" />
        </svg>
      );
    case "eye-off":
      return (
        <svg {...common}>
          <path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.5 18.5 0 0 1 5.06-5.94" />
          <path d="M9.9 4.24A9.1 9.1 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19" />
          <path d="m1 1 22 22" />
        </svg>
      );
    case "camera":
      // 主按钮：原实现用 2.2 描边（蓝色实底上要更重才不显细），保留原值。
      return (
        <svg {...common} strokeWidth={2.2}>
          <path d="M3 9a2 2 0 0 1 2-2h1.5l1.2-2h6.6l1.2 2H19a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
          <circle cx="12" cy="13" r="3.2" />
        </svg>
      );
    // ---- 配置窗左侧导航（原 App.tsx NAV_ICONS，2026-09-24 并入；path 逐字保留）----
    case "nav-system":
      return (
        <svg {...common}>
          <circle cx="12" cy="12" r="3" />
          <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
        </svg>
      );
    case "nav-capture":
      return (
        <svg {...common}>
          <path d="M6.13 1L6 16a2 2 0 0 0 2 2h15" />
          <path d="M1 6.13L16 6a2 2 0 0 1 2 2v15" />
        </svg>
      );
    case "nav-pin":
      return (
        <svg {...common}>
          <path d="M12 17v5" />
          <path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z" />
        </svg>
      );
    case "nav-clipboard":
      return (
        <svg {...common}>
          <rect x="8" y="2" width="8" height="4" rx="1" ry="1" />
          <path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2" />
          <path d="M12 11h4M12 16h4M8 11h.01M8 16h.01" />
        </svg>
      );
    case "nav-save":
      return (
        <svg {...common}>
          <path d="M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2z" />
          <polyline points="17 21 17 13 7 13 7 21" />
          <polyline points="7 3 7 8 15 8" />
        </svg>
      );
    case "nav-annotate":
      // 必须包 <svg>：原 NAV_ICONS 里这个节点是直接塞进 NavIcon 的 <svg> 里的，
      // 搬进 iconShape 后它就是返回值本身，裸 <path> 不会被渲染成图标（实测 10/11 缺标注）。
      return (
        <svg {...common}>
          <path d="M17 3a2.828 2.828 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z" />
        </svg>
      );
    case "nav-hotkeys":
      return (
        <svg {...common}>
          <rect x="2" y="6" width="20" height="12" rx="2" />
          <path d="M6 10h.01M10 10h.01M14 10h.01M18 10h.01M8 14h8" />
        </svg>
      );
    case "nav-builtin":
      return (
        <svg {...common}>
          <path d="M18 3a3 3 0 0 0-3 3v12a3 3 0 0 0 3 3 3 3 0 0 0 3-3 3 3 0 0 0-3-3H6a3 3 0 0 0-3 3 3 3 0 0 0 3 3 3 3 0 0 0 3-3V6a3 3 0 0 0-3-3 3 3 0 0 0-3 3 3 3 0 0 0 3 3h12a3 3 0 0 0 3-3 3 3 0 0 0-3-3z" />
        </svg>
      );
    case "nav-record":
      return (
        <svg {...common}>
          <circle cx="12" cy="12" r="10" />
          <circle cx="12" cy="12" r="3" fill="currentColor" stroke="none" />
        </svg>
      );
    case "nav-ocr":
      return (
        <svg {...common}>
          <path d="M3 7V5a2 2 0 0 1 2-2h2" />
          <path d="M17 3h2a2 2 0 0 1 2 2v2" />
          <path d="M21 17v2a2 2 0 0 1-2 2h-2" />
          <path d="M7 21H5a2 2 0 0 1-2-2v-2" />
          <path d="M7 8h10M7 12h10M7 16h6" />
        </svg>
      );
    case "nav-about":
      return (
        <svg {...common}>
          <circle cx="12" cy="12" r="10" />
          <line x1="12" y1="16" x2="12" y2="12" />
          <line x1="12" y1="8" x2="12.01" y2="8" />
        </svg>
      );
    default:
      return null;
  }
}

/** 图标组件。`name` 未知时返回 null（与覆盖层原实现一致，不抛错、不渲染占位）。
 * `strokeWidth` 用于覆盖层默认值（导航 1.6 / 主按钮 camera 自带 2.2）。 */
export function Icon({ name, size, strokeWidth }: { name: string; size?: number; strokeWidth?: number }) {
  const shape = iconShape(name, strokeWidth);
  if (!shape) return null;
  if (!size) return shape;
  return cloneElement(shape, { width: size, height: size });
}
