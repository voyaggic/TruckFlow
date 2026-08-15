import { useEffect, useState } from "react";
import { api } from "../lib/api";
import type { PasswordStrength } from "../lib/types";

export default function PasswordChecklist({ password }: { password: string }) {
  const [strength, setStrength] = useState<PasswordStrength | null>(null);

  useEffect(() => {
    let cancelled = false;
    api.validatePasswordStrength(password).then((s) => {
      if (!cancelled) setStrength(s);
    });
    return () => {
      cancelled = true;
    };
  }, [password]);

  if (!strength) return null;

  const items: Array<[string, boolean]> = [
    ["At least 8 characters", strength.length],
    ["One uppercase letter", strength.uppercase],
    ["One lowercase letter", strength.lowercase],
    ["One number", strength.digit],
    ["One symbol", strength.symbol],
  ];

  return (
    <div className="checklist">
      {items.map(([label, met]) => (
        <span key={label} className={met ? "check-item met" : "check-item"}>
          {label}
        </span>
      ))}
    </div>
  );
}
