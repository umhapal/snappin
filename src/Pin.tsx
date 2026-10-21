import { useCallback, useEffect, useRef, useState } from "react";
import type {
  CSSProperties,
  MouseEvent as RMouseEvent,
  SyntheticEvent,
  WheelEvent as RWheelEvent,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { DEFAULTS, type Settings } from "./settings";
// 圆角走 theme.css 的 token，必须先引 token 表——贴图窗是独立 webview，
// 漏引会让 var() 静默失效、圆角直接消失（剪贴板窗踩过同一坑）。
import "./theme.css";

/* ============================================================
 * SnapPin 贴图窗口（Pin）—— 由 main.tsx 按 label `pin-` 前缀分流。
 * 挂载后从后端取走本窗待显示的 PNG base64，铺满窗口渲染。
 * 交互：整窗左键拖动 / 滚轮缩放 / Ctrl+滚轮或 [ ] 调透明度 / 右键或 Esc 关闭。
 * 坐标与尺寸对齐：图的 naturalWidth/Height 视为逻辑（CSS）像素，用 LogicalSize 校正窗口。
 * 用户设置（`pin_opacity` / `pin_wheel_zoom` / `pin_shadow`）挂载时读一次，见下面的 effect。
 * ============================================================ */

const MIN_SCALE = 0.2;
const MAX_SCALE = 5;
// 透明度下限对齐设置里的取值范围（`pin_opacity` 1-100 → 0.01-1）：键盘调档与设置初值
// 用同一个下限，否则用户把设置调到 10% 后一按 `[` 就跳回 20%，看着像设置没生效。
const MIN_OPACITY = 0.01;
const MAX_OPACITY = 1;

// 本窗 label：由 main.tsx 分流保证以 `pin-` 开头。
const label = getCurrentWindow().label;

function Pin() {
  const [src, setSrc] = useState<string | null>(null);
  const [scale, setScale] = useState(1);
  const [opacity, setOpacity] = useState(1);
  // 用户设置：滚轮缩放开关（默认开）与阴影开关（默认开）。挂载时从后端拉一次，
  // 之后本窗不再跟设置联动（贴图是「钉住的一瞬间」的快照，中途改设置不该动已钉的图）。
  const [wheelZoom, setWheelZoom] = useState(true);
  const [shadow, setShadow] = useState(true);
  // 图的自然（逻辑）像素尺寸，onload 后确定。
  const natural = useRef<{ w: number; h: number }>({ w: 0, h: 0 });

  // 读用户设置：`pin_opacity` → 内容初始不透明度；`pin_wheel_zoom` → 滚轮缩放开关；
  // `pin_shadow` → 窗口阴影开关。拉不到就用 DEFAULTS（100% / 开 / 开），与改造前行为一致。
  useEffect(() => {
    let alive = true;
    invoke<Settings>("get_settings")
      .then((s) => {
        if (!alive) return;
        const merged = { ...DEFAULTS, ...s };
        const pct = Number(merged.pin_opacity);
        if (Number.isFinite(pct)) {
          setOpacity(Math.min(MAX_OPACITY, Math.max(MIN_OPACITY, pct / 100)));
        }
        setWheelZoom(merged.pin_wheel_zoom);
        setShadow(merged.pin_shadow);
      })
      .catch(() => {
        /* 拉不到设置就按默认：100% 不透明、滚轮缩放开、带阴影 */
      });
    return () => {
      alive = false;
    };
  }, []);

  // 关闭本窗。
  const close = useCallback(() => {
    void invoke("close_pin", { label });
  }, []);

  // 按当前 scale 把窗口内容尺寸设为 自然尺寸 * scale（逻辑像素）。
  const applySize = useCallback((s: number) => {
    const { w, h } = natural.current;
    if (w <= 0 || h <= 0) return;
    const cw = Math.max(1, Math.round(w * s));
    const ch = Math.max(1, Math.round(h * s));
    void getCurrentWindow().setSize(new LogicalSize(cw, ch));
  }, []);

  // 挂载后取图。
  useEffect(() => {
    let alive = true;
    invoke<string | null>("take_pin_payload", { label })
      .then((b64) => {
        if (!alive || !b64) return;
        const uri = b64.startsWith("data:") ? b64 : `data:image/png;base64,${b64}`;
        setSrc(uri);
      })
      .catch(() => {
        /* 取不到载荷（例如已被取走）时静默，窗口保持空 */
      });
    return () => {
      alive = false;
    };
  }, []);

  // 让 html/body 透明、去边距与滚动条，使窗口真正无背景。
  useEffect(() => {
    const html = document.documentElement;
    const body = document.body;
    const prev = {
      htmlBg: html.style.background,
      bodyBg: body.style.background,
      bodyMargin: body.style.margin,
      bodyOverflow: body.style.overflow,
    };
    html.style.background = "transparent";
    body.style.background = "transparent";
    body.style.margin = "0";
    body.style.overflow = "hidden";
    return () => {
      html.style.background = prev.htmlBg;
      body.style.background = prev.bodyBg;
      body.style.margin = prev.bodyMargin;
      body.style.overflow = prev.bodyOverflow;
    };
  }, []);

  // 键盘：Esc 关闭；[ / ] 调透明度。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        close();
      } else if (e.key === "[") {
        e.preventDefault();
        setOpacity((o) => Math.max(MIN_OPACITY, Number((o - 0.1).toFixed(2))));
      } else if (e.key === "]") {
        e.preventDefault();
        setOpacity((o) => Math.min(MAX_OPACITY, Number((o + 0.1).toFixed(2))));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [close]);

  // 图加载完成：记录自然尺寸并按初始 scale 校正窗口。
  const onImgLoad = useCallback(
    (e: SyntheticEvent<HTMLImageElement>) => {
      const img = e.currentTarget;
      natural.current = { w: img.naturalWidth, h: img.naturalHeight };
      applySize(scale);
    },
    [applySize, scale],
  );

  // 整窗左键拖动。
  const onMouseDown = useCallback((e: RMouseEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    void getCurrentWindow().startDragging();
  }, []);

  // 右键关闭。
  const onContextMenu = useCallback(
    (e: RMouseEvent<HTMLDivElement>) => {
      e.preventDefault();
      close();
    },
    [close],
  );

  // 滚轮：Ctrl+滚轮调透明度（不受开关影响）；普通滚轮缩放——受 `pin_wheel_zoom` 控制，
  // 关掉后滚轮在贴图上不做任何事（用户在设置里明确不要这个手势时不该还动尺寸）。
  const onWheel = useCallback(
    (e: RWheelEvent<HTMLDivElement>) => {
      if (e.ctrlKey) {
        const step = e.deltaY < 0 ? 0.05 : -0.05;
        setOpacity((o) =>
          Math.min(MAX_OPACITY, Math.max(MIN_OPACITY, Number((o + step).toFixed(2)))),
        );
        return;
      }
      if (!wheelZoom) return;
      const factor = e.deltaY < 0 ? 1.1 : 1 / 1.1;
      setScale((s) => {
        const next = Math.min(MAX_SCALE, Math.max(MIN_SCALE, s * factor));
        applySize(next);
        return next;
      });
    },
    [applySize, wheelZoom],
  );

  const containerStyle: CSSProperties = {
    position: "relative",
    width: "100vw",
    height: "100vh",
    margin: 0,
    padding: 0,
    overflow: "hidden",
    // 圆角（docs/UI设计规范.md §6「无边框圆角小窗」）：窗是 transparent + decorations(false)
    // （pin.rs::build_pin_window），角上露的是真桌面，所以这是真圆角而不是假装圆角。
    // overflow:hidden 已有，图片与阴影层随容器一起裁到同一圆角。
    borderRadius: "var(--radius-panel)",
    background: "transparent",
    cursor: "move",
    userSelect: "none",
  };

  // 阴影层（`pin_shadow`）：窗口尺寸**就是图片尺寸**（pin.rs 按图宽高建窗、透明无边框），
  // 外投影会被 webview 边界整条裁掉、根本看不见；inset 阴影又属于容器背景层，会被铺满的
  // 不透明图片整块盖住。所以单独叠一层在图**上面**画 inset 阴影：四周一圈暗边，
  // 关掉时图与桌面之间没有过渡边——两种状态肉眼可分。
  // pointerEvents:none 保证它不抢拖动/右键/滚轮。
  const shadowStyle: CSSProperties = {
    position: "absolute",
    inset: 0,
    pointerEvents: "none",
    boxShadow: "inset 0 0 14px 3px rgba(0, 0, 0, 0.55)",
  };

  const imgStyle: CSSProperties = {
    display: "block",
    width: "100%",
    height: "100%",
    objectFit: "fill",
    opacity,
    // 图不接收指针事件，left-drag/右键/滚轮全部落到容器上。
    pointerEvents: "none",
    userSelect: "none",
  };

  return (
    <div
      style={containerStyle}
      onMouseDown={onMouseDown}
      onContextMenu={onContextMenu}
      onWheel={onWheel}
    >
      {src && <img src={src} style={imgStyle} draggable={false} onLoad={onImgLoad} alt="pinned" />}
      {shadow && <div style={shadowStyle} aria-hidden />}
    </div>
  );
}

export default Pin;
