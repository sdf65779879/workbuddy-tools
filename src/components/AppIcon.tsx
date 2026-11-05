export type AppIconName =
  | "usage" | "vault" | "checkin" | "growth" | "gateway" | "share"
  | "requests" | "tokens" | "credits" | "latency" | "success"
  | "refresh" | "settings" | "gift" | "brand";

const assets: Record<AppIconName, string> = {
  usage: "usage", vault: "vault", checkin: "checkin", growth: "growth",
  gateway: "gateway", share: "share", requests: "gateway", tokens: "credits",
  credits: "credits", latency: "growth", success: "checkin", refresh: "usage",
  settings: "vault", gift: "credits", brand: "gateway",
};

/** Original transparent 2D assets generated with the built-in imagegen tool. */
export default function AppIcon({ name, size = 24, className = "" }: {
  name: AppIconName;
  size?: number;
  className?: string;
}) {
  return <img src={`/icons/${assets[name]}.png`} width={size} height={size}
    className={`wb-icon ${className}`} alt="" aria-hidden="true" draggable={false}
    style={{ width: size, height: size }} />;
}

