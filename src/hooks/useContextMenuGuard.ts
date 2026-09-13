import React from "react";

/**
 * Props for the two halves of a Radix context menu — spread `triggerProps` on
 * `<ContextMenuTrigger>` and `contentProps` on `<ContextMenuContent>`.
 */
export type ContextMenuGuard = {
  triggerProps: { onContextMenu: () => void };
  contentProps: {
    onPointerDownCapture: () => void;
    onPointerUpCapture: (event: React.PointerEvent) => void;
  };
};

/**
 * Swallows the release of the very right-click that OPENED the menu, so that
 * click cannot also select an item.
 *
 * On macOS WebKit `contextmenu` fires on right-button DOWN, so the content is
 * already mounted when the button comes back UP. Normally that release lands
 * on the content's own padding — Radix anchors a context menu's top-left
 * corner at the pointer — but near the BOTTOM of the viewport the collision
 * shift slides the content up until it fits, which puts a menu ITEM under the
 * cursor. `MenuItem` clicks any item that receives a `pointerup` with no
 * matching `pointerdown` (that is its press-and-drag-to-select affordance), so
 * the release fires whatever landed there: the menu appears never to open, and
 * a row's "Migrate…" / "Delete…" runs instead. Worst near the bottom of a
 * scrolled list, which is exactly where the last rows sit.
 *
 * The cost is press-right-drag-release selection, which this app never
 * offered; a press INSIDE the menu disarms the guard, so ordinary clicking is
 * untouched.
 */
export function useContextMenuGuard(): ContextMenuGuard {
  const armed = React.useRef(false);
  return React.useMemo(
    () => ({
      triggerProps: {
        // Runs before Radix's own handler, which is what opens the menu.
        onContextMenu: () => {
          armed.current = true;
        }
      },
      contentProps: {
        // A press inside the menu is a real selection gesture — let its
        // release through.
        onPointerDownCapture: () => {
          armed.current = false;
        },
        onPointerUpCapture: (event: React.PointerEvent) => {
          if (!armed.current) return;
          armed.current = false;
          // Capture phase: stops the event before MenuItem's own handler can
          // turn it into a click.
          event.preventDefault();
          event.stopPropagation();
        }
      }
    }),
    []
  );
}
