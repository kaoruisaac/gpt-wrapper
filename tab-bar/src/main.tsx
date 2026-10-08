import { For, batch, createEffect, createMemo, createSignal, onCleanup, onMount } from "solid-js";
import { render } from "solid-js/web";
import { invoke } from "@tauri-apps/api/core";
import { FiPlus, FiX } from "solid-icons/fi";
import "./styles.css";

type TabInfo = {
  id: number;
  title: string;
  status: "working" | "send" | "voice" | "fallback";
};

type TabSnapshot = {
  tabs: TabInfo[];
  activeTabId: number | null;
};

type TabHostApi = {
  refresh: () => Promise<void>;
};

declare global {
  interface Window {
    __GPT_WRAPPER_TAB_HOST__?: TabHostApi;
  }
}

const EMPTY_SNAPSHOT: TabSnapshot = {
  tabs: [],
  activeTabId: null,
};

const LONG_PRESS_MS = 400;
const DRAG_THRESHOLD_PX = 5;
const EDGE_SCROLL_ZONE_PX = 32;
const MAX_EDGE_SCROLL_PX = 14;
const SETTLE_DURATION_MS = 185;
const EASING = "cubic-bezier(0.2, 0, 0, 1)";

type DragPreview = {
  tabId: number;
  offsets: Map<number, number>;
};

type DragSession = {
  pointerId: number;
  tabId: number;
  element: HTMLButtonElement;
  container: HTMLDivElement;
  initialIds: number[];
  initialIndex: number;
  lefts: Map<number, number>;
  widths: Map<number, number>;
  gap: number;
  startClientX: number;
  startClientY: number;
  startContentX: number;
  latestClientX: number;
  currentIndex: number;
  initiated: boolean;
  scrollFrame: number;
};

function moveTab(ids: number[], from: number, to: number): number[] {
  const next = [...ids];
  next.splice(to, 0, next.splice(from, 1)[0]);
  return next;
}

function sameOrder(left: number[], right: number[]): boolean {
  return left.length === right.length && left.every((id, index) => id === right[index]);
}

function TabBarApp() {
  const [snapshot, setSnapshot] = createSignal<TabSnapshot>(EMPTY_SNAPSHOT);
  const [busy, setBusy] = createSignal(false);
  const [dragPreview, setDragPreview] = createSignal<DragPreview | null>(null);
  const tabIds = createMemo(() => snapshot().tabs.map((tab) => tab.id));
  let tabStrip: HTMLDivElement | undefined;
  let gesture: { pointerId: number; startedAt: number; timer: ReturnType<typeof setTimeout> } | null = null;
  let drag: DragSession | null = null;
  let suppressClick: { tabId: number; until: number } | null = null;
  let revision = 0;
  let reorderPending = false;

  const tabById = (id: number) => snapshot().tabs.find((tab) => tab.id === id);

  // Ignore stale background snapshots while an optimistic reorder is being committed.
  const applySnapshot = (next: TabSnapshot) => {
    if (drag && !sameOrder(drag.initialIds, next.tabs.map((tab) => tab.id))) {
      cancelDrag();
    }
    setSnapshot(next);
  };

  const refresh = async () => {
    const expectedRevision = revision;
    try {
      const next = await invoke<TabSnapshot>("get_tabs");
      if (expectedRevision !== revision || reorderPending) return;
      applySnapshot(next);
    } catch (error) {
      console.error("[gpt-wrapper] failed to refresh tabs:", error);
    }
  };

  const runCommand = async (
    command: "create_tab" | "create_project_tab" | "switch_tab" | "close_tab",
    args: Record<string, unknown> = {},
  ) => {
    if (busy() || drag) return;

    revision++;
    setBusy(true);
    try {
      applySnapshot(await invoke<TabSnapshot>(command, args));
    } catch (error) {
      console.error(`[gpt-wrapper] ${command} failed:`, error);
    } finally {
      setBusy(false);
    }
  };

  // Animate from current on-screen positions to the resulting flex layout (FLIP).
  // This also makes an Escape/cancel spring back rather than snap abruptly.
  const settleTo = (next: TabSnapshot) => {
    const container = tabStrip;
    if (!container) {
      batch(() => {
        setDragPreview(null);
        setSnapshot(next);
      });
      return;
    }
    const before = new Map<number, number>();
    for (const element of container.querySelectorAll<HTMLElement>("[data-tab-id]")) {
      before.set(Number(element.dataset.tabId), element.getBoundingClientRect().left);
    }

    container.classList.add("is-settling");
    batch(() => {
      setDragPreview(null);
      setSnapshot(next);
    });

    for (const element of container.querySelectorAll<HTMLElement>("[data-tab-id]")) {
      const previousLeft = before.get(Number(element.dataset.tabId));
      if (previousLeft === undefined) continue;
      const offset = previousLeft - element.getBoundingClientRect().left;
      if (Math.abs(offset) < 0.5) continue;
      element.animate(
        [{ transform: `translate3d(${offset}px, 0, 0)` }, { transform: "translate3d(0, 0, 0)" }],
        { duration: SETTLE_DURATION_MS, easing: EASING },
      );
    }
    requestAnimationFrame(() => container.classList.remove("is-settling"));
  };

  const releaseDrag = (session: DragSession) => {
    cancelAnimationFrame(session.scrollFrame);
    drag = null;
    if (session.element.hasPointerCapture(session.pointerId)) {
      session.element.releasePointerCapture(session.pointerId);
    }
  };

  const cancelDrag = () => {
    const session = drag;
    if (!session) return;
    if (session.initiated) {
      suppressClick = { tabId: session.tabId, until: performance.now() + 350 };
    }
    releaseDrag(session);
    if (session.initiated) settleTo(snapshot());
  };

  const updateDrag = (session: DragSession) => {
    const containerRect = session.container.getBoundingClientRect();
    const contentX = session.latestClientX - containerRect.left + session.container.scrollLeft;
    const deltaX = contentX - session.startContentX;
    const draggedCenter = (session.lefts.get(session.tabId) ?? 0)
      + (session.widths.get(session.tabId) ?? 0) / 2 + deltaX;

    let nextIndex = 0;
    for (const id of session.initialIds) {
      if (id === session.tabId) continue;
      const midpoint = (session.lefts.get(id) ?? 0) + (session.widths.get(id) ?? 0) / 2;
      if (draggedCenter > midpoint) nextIndex++;
    }
    session.currentIndex = nextIndex;

    const ordered = moveTab(session.initialIds, session.initialIndex, nextIndex);
    const offsets = new Map<number, number>();
    let nextLeft = session.lefts.get(session.initialIds[0]) ?? 0;
    for (const id of ordered) {
      offsets.set(id, id === session.tabId ? deltaX : nextLeft - (session.lefts.get(id) ?? 0));
      nextLeft += (session.widths.get(id) ?? 0) + session.gap;
    }
    setDragPreview({ tabId: session.tabId, offsets });
  };

  const edgeScroll = () => {
    const session = drag;
    if (!session || !session.initiated) return;
    const container = session.container;
    const bounds = container.getBoundingClientRect();
    const leftDistance = session.latestClientX - bounds.left;
    const rightDistance = bounds.right - session.latestClientX;
    let velocity = 0;

    if (leftDistance < EDGE_SCROLL_ZONE_PX) {
      velocity = -MAX_EDGE_SCROLL_PX * Math.min(1, (EDGE_SCROLL_ZONE_PX - leftDistance) / EDGE_SCROLL_ZONE_PX);
    } else if (rightDistance < EDGE_SCROLL_ZONE_PX) {
      velocity = MAX_EDGE_SCROLL_PX * Math.min(1, (EDGE_SCROLL_ZONE_PX - rightDistance) / EDGE_SCROLL_ZONE_PX);
    }

    const previous = container.scrollLeft;
    container.scrollLeft += velocity;
    if (container.scrollLeft !== previous) {
      updateDrag(session);
      session.scrollFrame = requestAnimationFrame(edgeScroll);
    } else {
      session.scrollFrame = 0;
    }
  };

  const startEdgeScroll = (session: DragSession) => {
    if (!session.scrollFrame) session.scrollFrame = requestAnimationFrame(edgeScroll);
  };

  const startTabDrag = (event: PointerEvent, tabId: number) => {
    if (busy() || drag || !event.isPrimary || event.button !== 0 || !tabStrip) return;
    if ((event.target as Element).closest(".gpt-wrapper-tab-close")) return;

    const element = event.currentTarget as HTMLButtonElement;
    const ids = tabIds();
    const initialIndex = ids.indexOf(tabId);
    if (initialIndex < 0) return;

    const containerRect = tabStrip.getBoundingClientRect();
    const lefts = new Map<number, number>();
    const widths = new Map<number, number>();
    for (const item of tabStrip.querySelectorAll<HTMLElement>("[data-tab-id]")) {
      const rect = item.getBoundingClientRect();
      const id = Number(item.dataset.tabId);
      lefts.set(id, rect.left - containerRect.left + tabStrip.scrollLeft);
      widths.set(id, rect.width);
    }
    const first = ids[0];
    const second = ids[1];
    const gap = second === undefined ? 4
      : (lefts.get(second) ?? 0) - (lefts.get(first) ?? 0) - (widths.get(first) ?? 0);

    drag = {
      pointerId: event.pointerId,
      tabId,
      element,
      container: tabStrip,
      initialIds: [...ids],
      initialIndex,
      lefts,
      widths,
      gap,
      startClientX: event.clientX,
      startClientY: event.clientY,
      startContentX: event.clientX - containerRect.left + tabStrip.scrollLeft,
      latestClientX: event.clientX,
      currentIndex: initialIndex,
      initiated: false,
      scrollFrame: 0,
    };
    element.setPointerCapture(event.pointerId);
  };

  const movePointer = (event: PointerEvent) => {
    const session = drag;
    if (!session || event.pointerId !== session.pointerId) return;
    session.latestClientX = event.clientX;
    if (!session.initiated) {
      const traveled = Math.hypot(event.clientX - session.startClientX, event.clientY - session.startClientY);
      if (traveled < DRAG_THRESHOLD_PX) return;
      session.initiated = true;
    }
    event.preventDefault();
    updateDrag(session);
    startEdgeScroll(session);
  };

  const finishTabDrag = (event: PointerEvent) => {
    const session = drag;
    if (!session || event.pointerId !== session.pointerId) return;
    const wasDragging = session.initiated;
    const orderedIds = moveTab(session.initialIds, session.initialIndex, session.currentIndex);
    if (wasDragging) suppressClick = { tabId: session.tabId, until: performance.now() + 350 };
    releaseDrag(session);

    if (!wasDragging) return;
    if (event.type !== "pointerup" || sameOrder(orderedIds, session.initialIds)) {
      settleTo(snapshot());
      return;
    }

    // Reorder only the labels locally, not their associated WebViews.
    const byId = new Map(snapshot().tabs.map((tab) => [tab.id, tab]));
    const next: TabSnapshot = {
      ...snapshot(),
      tabs: orderedIds.map((id) => byId.get(id)!).filter((tab): tab is TabInfo => !!tab),
    };
    revision++;
    reorderPending = true;
    setBusy(true);
    settleTo(next);
    void (async () => {
      try {
        applySnapshot(await invoke<TabSnapshot>("reorder_tabs", { tabIds: orderedIds }));
      } catch (error) {
        console.error("[gpt-wrapper] failed to reorder tabs:", error);
        try {
          settleTo(await invoke<TabSnapshot>("get_tabs"));
        } catch (refreshError) {
          console.error("[gpt-wrapper] failed to recover tab order:", refreshError);
        }
      } finally {
        reorderPending = false;
        setBusy(false);
        void refresh();
      }
    })();
  };

  const clearGesture = (button?: HTMLButtonElement) => {
    if (!gesture) return;
    clearTimeout(gesture.timer);
    if (button?.hasPointerCapture(gesture.pointerId)) {
      button.releasePointerCapture(gesture.pointerId);
    }
    gesture = null;
  };

  const startNewTabGesture = (event: PointerEvent) => {
    if (busy() || drag || gesture || !event.isPrimary || event.button !== 0) return;
    event.preventDefault();
    const button = event.currentTarget as HTMLButtonElement;
    button.setPointerCapture(event.pointerId);
    const pointerId = event.pointerId;
    const timer = setTimeout(() => {
      if (!gesture || gesture.pointerId !== pointerId) return;
      // After the threshold, this gesture cannot fall back to a Default tab.
      gesture = null;
      setBusy(true);
      const rect = button.getBoundingClientRect();
      if (button.hasPointerCapture(pointerId)) button.releasePointerCapture(pointerId);
      void (async () => {
        try {
          const projectId = await invoke<string | null>("show_project_bookmark_menu", {
            anchor: { right: rect.right, bottom: rect.bottom },
          });
          if (projectId) {
            revision++;
            applySnapshot(await invoke<TabSnapshot>("create_project_tab", { projectId }));
          }
        } catch (error) {
          console.error("[gpt-wrapper] bookmark menu failed:", error);
        } finally {
          setBusy(false);
        }
      })();
    }, LONG_PRESS_MS);
    gesture = { pointerId, startedAt: performance.now(), timer };
  };

  const finishNewTabGesture = (event: PointerEvent) => {
    if (!gesture || gesture.pointerId !== event.pointerId) return;
    const button = event.currentTarget as HTMLButtonElement;
    const isShortPress = performance.now() - gesture.startedAt < LONG_PRESS_MS;
    clearGesture(button);
    if (event.type === "pointerup" && isShortPress) void runCommand("create_tab");
  };

  const closeTab = (event: MouseEvent | KeyboardEvent, tabId: number) => {
    event.preventDefault();
    event.stopPropagation();
    void runCommand("close_tab", { tabId });
  };

  const handleCloseKeyDown = (event: KeyboardEvent, tabId: number) => {
    if (event.key === "Enter" || event.key === " ") {
      closeTab(event, tabId);
    }
  };

  createEffect(() => {
    const activeTabId = snapshot().activeTabId;
    if (activeTabId === null || drag || reorderPending) return;

    queueMicrotask(() => {
      if (drag || reorderPending) return;
      document.querySelector<HTMLElement>(`[data-tab-id="${activeTabId}"]`)
        ?.scrollIntoView({ block: "nearest", inline: "nearest" });
    });
  });

  onMount(() => {
    const hostApi: TabHostApi = { refresh };
    window.__GPT_WRAPPER_TAB_HOST__ = hostApi;
    window.addEventListener("pointermove", movePointer);
    window.addEventListener("pointerup", finishTabDrag);
    window.addEventListener("pointercancel", finishTabDrag);
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("resize", cancelDrag);
    void refresh();

    onCleanup(() => {
      cancelDrag();
      clearGesture();
      window.removeEventListener("pointermove", movePointer);
      window.removeEventListener("pointerup", finishTabDrag);
      window.removeEventListener("pointercancel", finishTabDrag);
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("resize", cancelDrag);
      if (window.__GPT_WRAPPER_TAB_HOST__ === hostApi) {
        delete window.__GPT_WRAPPER_TAB_HOST__;
      }
    });
  });

  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Escape" && drag) {
      event.preventDefault();
      cancelDrag();
    }
  };

  return (
    <div id="gpt-wrapper-tab-bar" classList={{ "is-busy": busy(), "is-reordering": !!dragPreview() }}>
      <div class="gpt-wrapper-tabs" role="tablist" aria-label="ChatGPT tabs" ref={tabStrip}>
        <For each={tabIds()}>
          {(tabId) => {
            const tab = () => tabById(tabId);
            const isActive = () => tabId === snapshot().activeTabId;
            const isDragging = () => dragPreview()?.tabId === tabId;

            return (
              <button
                class="gpt-wrapper-tab"
                classList={{ "is-active": isActive(), "is-dragging": isDragging() }}
                type="button"
                data-tab-id={tabId}
                aria-current={isActive() ? "page" : "false"}
                title={tab()?.title ?? "ChatGPT"}
                style={{ transform: dragPreview() ? `translate3d(${dragPreview()!.offsets.get(tabId) ?? 0}px,0,0)` : undefined }}
                onPointerDown={(event) => startTabDrag(event, tabId)}
                onLostPointerCapture={(event) => {
                  if (drag?.pointerId === event.pointerId) cancelDrag();
                }}
                onClick={() => {
                  if (suppressClick?.tabId === tabId && performance.now() < suppressClick.until) {
                    suppressClick = null;
                    return;
                  }
                  if (!busy() && !drag && !isActive()) {
                    void runCommand("switch_tab", { tabId });
                  }
                }}
              >
                <span class="gpt-wrapper-tab-dot" data-status={tab()?.status ?? "fallback"} aria-hidden="true" />
                <span class="gpt-wrapper-tab-title">{tab()?.title ?? "ChatGPT"}</span>
                <span
                  class="gpt-wrapper-tab-close"
                  role="button"
                  tabIndex={0}
                  aria-label={`Close ${tab()?.title ?? "ChatGPT"}`}
                  title="Close tab"
                  onClick={(event) => closeTab(event, tabId)}
                  onKeyDown={(event) => handleCloseKeyDown(event, tabId)}
                >
                  <FiX aria-hidden="true" />
                </span>
              </button>
            );
          }}
        </For>
      </div>

      <button
        class="gpt-wrapper-new-tab"
        type="button"
        aria-label="Open new ChatGPT tab"
        title="New ChatGPT tab"
        disabled={busy()}
        onPointerDown={startNewTabGesture}
        onPointerUp={finishNewTabGesture}
        onPointerCancel={finishNewTabGesture}
        onLostPointerCapture={finishNewTabGesture}
        onClick={(event) => {
          if (event.detail === 0) void runCommand("create_tab");
        }}
      >
        <FiPlus aria-hidden="true" />
      </button>
    </div>
  );
}

const root = document.getElementById("root");
if (!root) {
  throw new Error("Tab bar root element is missing.");
}

render(() => <TabBarApp />, root);
