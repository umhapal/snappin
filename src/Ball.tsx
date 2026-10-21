import { useCallback, useEffect, useRef } from "react";
import type { MouseEvent as RMouseEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
// Ball 的样式定义在 Overlay.css（.ball-* 在覆盖层那份里），原本只靠 main.tsx 导入 Overlay
// 的副作用带进来——单独加载 Ball 页会**裸奔无样式**（预览台实测 0 边框 0 圆角）。自己引一遍。
import "./theme.css";
import "./Overlay.css";
import { Icon } from "./icons";

/* ============================================================
 * SnapPin 桌面悬浮条（docs/UI设计规范.md §1.5 real-7，照 PixPin 形态）：
 * 白底圆角横条 + 蓝色圆形主按钮，常驻屏幕边缘。
 *
 * 与 PixPin 的差异（有意为之）：PixPin 那条里有「收藏 / 历史」，本工具这两个能力
 * 还没实现，所以这里只放**真实可用**的三个动作（截图 / 设置 / 隐藏）——
 * 不再走「界面摆着、点了没用」的老路（用户 2026-09-21 为此踩过坑）。
 *
 * 窗口 = 横条本身的大小（透明像素也吃鼠标事件，常驻大窗会挡住下面的桌面）。
 * 拖动 = 条身任意处（含按钮）按住移动 >4px 才算拖，原地松手仍是点击；
 * 松手把位置写回配置，下次启动原位出现。
 * hover 介绍走原生 title：Ball 页没有 Overlay 那套 data-tip 渲染器，
 * data-tip 在这里是死属性（BallBugScout 2026-09-23 定位）。
 * ============================================================ */

function Ball() {
  const win = getCurrentWindow();
  // 拖动结束后短暂吞 click：OS 拖动循环若仍补发 click，别把「拖」误触发成「点按钮」。
  const suppressClick = useRef(false);

  // 等窗口位置连续 3 次采样（100ms）不变 = 拖动真正结束。
  // 本机 startDragging 不阻塞到松手（2026-09-23 实测：立刻读 outerPosition 拿到的是
  // 抓起点，写回等于没改 → 位置持久化失效、check-15 拖动断言超时）。
  const waitStablePos = useCallback(async () => {
    const scale = await win.scaleFactor();
    let last = await win.outerPosition();
    let same = 0;
    for (let i = 0; i < 150 && same < 3; i++) {
      const { promise, resolve } = Promise.withResolvers<void>();
      setTimeout(resolve, 100);
      await promise;
      const p = await win.outerPosition();
      if (p.x === last.x && p.y === last.y) same++;
      else {
        same = 0;
        last = p;
      }
    }
    return { x: Math.round(last.x / scale), y: Math.round(last.y / scale) };
  }, [win]);

  // 拖动：旧版按钮 mousedown 全 stopPropagation、按钮又铺满条身，
  // 可拖区只剩 6-9px 缝隙（用户实测「拖不动」）——现改为阈值拖动，全条身可拖。
  const onMouseDown = useCallback(
    (e: RMouseEvent<HTMLDivElement>) => {
      if (e.button !== 0) return;
      const sx = e.clientX;
      const sy = e.clientY;
      let started = false;
      function cleanup() {
        window.removeEventListener("mousemove", onMove);
        window.removeEventListener("mouseup", onMouseUp);
      }
      function onMove(ev: MouseEvent) {
        if (started) return;
        if (Math.abs(ev.clientX - sx) < 4 && Math.abs(ev.clientY - sy) < 4) return;
        started = true;
        cleanup();
        // 过阈值立刻吞 click：mouseup 可能早于下面「位置稳定」判定完成。
        suppressClick.current = true;
        void (async () => {
          await win.startDragging(); // 本机瞬返，结束与否看 waitStablePos
          const pos = await waitStablePos();
          void invoke("set_ball_pos", { x: pos.x, y: pos.y }).catch(() => {});
        })()
          .catch((e) => console.error("球条拖动失败", e))
          .finally(() => {
            setTimeout(() => (suppressClick.current = false), 400);
          });
      }
      function onMouseUp() {
        cleanup();
      }
      window.addEventListener("mousemove", onMove);
      window.addEventListener("mouseup", onMouseUp);
    },
    [win, waitStablePos],
  );

  // 拖动后补发的 click 一律吞掉；原地点击没走过拖动路径，不受影响。
  const onClickCapture = useCallback((e: RMouseEvent<HTMLDivElement>) => {
    if (suppressClick.current) {
      e.preventDefault();
      e.stopPropagation();
    }
  }, []);

  const shot = useCallback(() => {
    // 走 trigger_capture_now（后端排队到主线程再建窗）：直接在 IPC 回调里建 overlay
    // 会让新 webview 停在 about:blank，见后端注释。
    void invoke("trigger_capture_now").catch(() => {});
  }, []);

  const openSettings = useCallback(() => {
    void invoke("show_main_window").catch(() => {});
  }, []);

  const hide = useCallback(() => {
    void invoke("close_ball").catch(() => {});
  }, []);

  // 让 html/body 透明、去边距，横条才是真正的圆角白条（同 Pin.tsx 做法）。
  useEffect(() => {
    const html = document.documentElement;
    const body = document.body;
    html.style.background = "transparent";
    body.style.background = "transparent";
    body.style.margin = "0";
    body.style.overflow = "hidden";
  }, []);

  return (
    <div
      className="ball-bar"
      onMouseDown={onMouseDown}
      onClickCapture={onClickCapture}
      onContextMenu={(e) => {
        e.preventDefault();
        hide();
      }}
      title="拖动可移动 · 右键隐藏"
    >
      <button
        type="button"
        className="ball-act"
        onClick={shot}
        aria-label="截图"
        title="截图"
      >
        <Icon name="capture" />
      </button>

      <button
        type="button"
        className="ball-act"
        onClick={openSettings}
        aria-label="打开设置"
        title="打开设置"
      >
        <Icon name="settings" />
      </button>

      <button
        type="button"
        className="ball-act"
        onClick={hide}
        aria-label="隐藏悬浮条"
        title="隐藏悬浮条"
      >
        <Icon name="eye-off" />
      </button>

      {/* 蓝色圆形主按钮（PixPin 形态）：点它直接截图 */}
      <button
        type="button"
        className="ball-main"
        onClick={shot}
        aria-label="截图"
        title="截图"
      >
        <Icon name="camera" />
      </button>
    </div>
  );
}

export default Ball;
