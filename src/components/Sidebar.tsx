import { useTranslation } from "react-i18next";
import AppIcon, { type AppIconName } from "./AppIcon";

export type Page = "overview" | "vault" | "checkin" | "growth" | "gateway" | "share";

const items: { id: Page; icon: AppIconName; labelKey: string }[] = [
  { id: "overview", icon: "usage", labelKey: "nav.usage" },
  { id: "vault", icon: "vault", labelKey: "nav.vault" },
  { id: "checkin", icon: "checkin", labelKey: "nav.checkin" },
  { id: "growth", icon: "growth", labelKey: "nav.growth" },
  { id: "gateway", icon: "gateway", labelKey: "nav.gateway" },
  { id: "share", icon: "share", labelKey: "nav.share" },
];

export default function Sidebar({ page, onNavigate }: {
  page: Page; onNavigate: (page: Page) => void;
}) {
  const { t } = useTranslation();
  return <nav className="wb-navigation" aria-label="主导航">
    {items.map((item) => <button key={item.id} type="button"
      aria-current={page === item.id ? "page" : undefined}
      onClick={() => onNavigate(item.id)}
      className={`wb-nav-item ${page === item.id ? "is-active" : ""}`}>
      <AppIcon name={item.icon} size={21} />
      <span>{t(item.labelKey)}</span>
    </button>)}
  </nav>;
}

