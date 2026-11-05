import { getCurrentWindow } from "@tauri-apps/api/window";

import { Back, Close, Gear, Minus } from "./icons";
import { Strings } from "./i18n";

/** Barra propia (la ventana no tiene decoracion): la zona vacia arrastra, y
 *  cerrar manda la app a la bandeja, igual que el CloseRequested del backend. */
export function TitleBar(props: { t: Strings; inSettings: boolean; onSettings: () => void; onBack: () => void }) {
  const { t, inSettings } = props;
  // Fuera de Tauri (navegador suelto) no hay ventana: los botones no hacen nada.
  const win = () => {
    try {
      return getCurrentWindow();
    } catch {
      return null;
    }
  };
  return (
    <div className="titlebar" data-tauri-drag-region>
      {inSettings && (
        <button className="icon-btn" title={t.back} onClick={props.onBack}>
          <Back size={14} />
        </button>
      )}
      <span className="title">{inSettings ? t.settings : t.appTitle}</span>
      <div className="window-buttons">
        {!inSettings && (
          <button className="icon-btn" title={t.settings} onClick={props.onSettings}>
            <Gear size={14} />
          </button>
        )}
        <button className="icon-btn" title={t.minimize} onClick={() => void win()?.minimize()}>
          <Minus size={14} />
        </button>
        <button className="icon-btn close" title={t.close} onClick={() => void win()?.hide()}>
          <Close size={14} />
        </button>
      </div>
    </div>
  );
}
