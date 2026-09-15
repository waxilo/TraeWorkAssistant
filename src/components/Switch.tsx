/**
 * 统一开关控件：替代裸 checkbox。
 * 用于「启用定时签到」「开启智能接管」「账号启用」等开关型设置。
 */
interface Props {
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  /** sm = 表格/紧凑场景；lg = 页面主控件（整页只有一个开关时用） */
  size?: "sm" | "md" | "lg";
  title?: string;
}

export default function Switch({ checked, onChange, disabled, size = "md", title }: Props) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      title={title}
      className={`switch ${size}${checked ? " on" : ""}`}
      disabled={disabled}
      onClick={() => !disabled && onChange(!checked)}
    >
      <span className="knob" />
    </button>
  );
}
