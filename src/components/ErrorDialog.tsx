import { useEffect, useRef } from "react";

export default function ErrorDialog({ error, onDismiss }: { error: string; onDismiss: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    dialog.current?.showModal();
  }, []);

  return <dialog ref={dialog} className="wb-error-dialog" role="alertdialog" aria-modal="true"
    aria-labelledby="wb-error-title" aria-describedby="wb-error-message"
    onCancel={(event) => { event.preventDefault(); onDismiss(); }}>
    <div className="wb-error-heading">
      <span className="wb-error-symbol" aria-hidden="true">!</span>
      <h2 id="wb-error-title">执行失败</h2>
      <button type="button" className="btn btn-ghost btn-xs wb-error-close" onClick={onDismiss} aria-label="关闭错误提示">×</button>
    </div>
    <p className="wb-error-message" id="wb-error-message">{error}</p>
    <div className="wb-error-actions"><button type="button" className="btn btn-primary btn-sm" onClick={onDismiss} autoFocus>知道了</button></div>
  </dialog>;
}
