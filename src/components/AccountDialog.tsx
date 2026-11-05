import { useEffect, useRef, type ReactNode } from "react";

/** Native modal semantics keep focus inside the account flow and the page inert. */
export default function AccountDialog({ children, onDismiss, titleId, descriptionId, returnFocusTo, compact = false }: {
  children: ReactNode;
  onDismiss: () => void;
  titleId: string;
  descriptionId?: string;
  returnFocusTo?: HTMLElement | null;
  compact?: boolean;
}) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const dismiss = useRef(onDismiss);
  dismiss.current = onDismiss;

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    const trigger = returnFocusTo ?? (document.activeElement instanceof HTMLElement ? document.activeElement : null);
    const previousOverflow = document.body.style.overflow;
    dialog.showModal();
    dialog.querySelector<HTMLElement>("[data-dialog-initial-focus]")?.focus({ preventScroll: true });
    document.body.style.overflow = "hidden";
    return () => {
      if (dialog.open) dialog.close();
      document.body.style.overflow = previousOverflow;
      if (trigger?.isConnected) trigger.focus({ preventScroll: true });
    };
  }, [returnFocusTo]);

  return <dialog ref={dialogRef} className={`wb-account-dialog${compact ? " wb-account-dialog-compact" : ""}`}
    aria-labelledby={titleId} aria-describedby={descriptionId}
    onCancel={(event) => { event.preventDefault(); dismiss.current(); }}
    onClick={(event) => {
      if (event.target !== event.currentTarget) return;
      const box = event.currentTarget.getBoundingClientRect();
      if (event.clientX < box.left || event.clientX > box.right || event.clientY < box.top || event.clientY > box.bottom) {
        dismiss.current();
      }
    }}>
    {children}
  </dialog>;
}
