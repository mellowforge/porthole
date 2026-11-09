export interface ButtonProps {
  variant: "primary" | "secondary" | "ghost";
  label: string;
  shortcut?: string;
  onClick?: () => void;
  disabled?: boolean;
  title?: string;
  danger?: boolean;
  /** URL of a small icon shown before the label. */
  icon?: string;
}

export function Button({ variant, label, shortcut, onClick, disabled, title, danger, icon }: ButtonProps) {
  return (
    <button
      type="button"
      className="btn"
      data-variant={variant}
      data-danger={danger ? "true" : undefined}
      disabled={disabled}
      title={title}
      onClick={onClick}
    >
      {icon && <img className="btn-icon" src={icon} alt="" />}
      <span>{label}</span>
      {shortcut && <kbd>{shortcut}</kbd>}
    </button>
  );
}
