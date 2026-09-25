// The platform cost report returned by GET /v1/platform/costs.
export type CostBasis = "metered" | "allocated" | "estimated" | "fixed";

export type CostLineItem = {
  item: string;
  provider: "cloudflare" | "workos";
  label: string;
  unit: string;
  usd: number;
  per: number;
  per_label: string;
  basis: CostBasis;
  quantity: number;
  cost_usd: number;
};

export type CostSummary = {
  cloudflare_usd: number;
  workos_usd: number;
  total_usd: number;
  line_items: CostLineItem[];
};

export type CompanyCost = CostSummary & { company_id: string; name: string };

export type CostReport = {
  month: string;
  start_date: string;
  end_date: string;
  complete: boolean;
  pricing_as_of: string;
  total_usd: number;
  companies: CompanyCost[];
  platform: CostSummary;
  warnings: string[];
};

export const BASIS_DESCRIPTIONS: Record<CostBasis, string> = {
  metered: "Counted for this company",
  allocated: "A shared total divided by this company's share of the usage",
  estimated: "Derived from a related count; the provider reports no exact figure",
  fixed: "A flat platform fee",
};

// The server reports the current and previous two UTC months.
export function recentMonths(now: Date, count = 3) {
  return Array.from({ length: count }, (_, index) => {
    const month = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() - index, 1));
    return `${month.getUTCFullYear()}-${String(month.getUTCMonth() + 1).padStart(2, "0")}`;
  });
}

export function monthLabel(month: string) {
  const [year, number] = month.split("-").map(Number);
  return new Date(Date.UTC(year, number - 1, 1)).toLocaleDateString("en-US", { month: "long", year: "numeric", timeZone: "UTC" });
}

const dollars = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });
const smallDollars = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD", maximumSignificantDigits: 2 });

// Totals round to cents; anything above zero stays visible.
export function formatUsd(amount: number) {
  if (amount > 0 && amount < 0.005) return "<$0.01";
  return dollars.format(amount);
}

// Line items keep enough precision to compare small usage-based charges.
export function formatPreciseUsd(amount: number) {
  if (amount === 0 || amount >= 0.1) return dollars.format(amount);
  return smallDollars.format(amount);
}

export function formatRate(line: Pick<CostLineItem, "usd" | "per_label">) {
  const price = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD", maximumFractionDigits: 3 }).format(line.usd);
  return `${price} / ${line.per_label}`;
}

export function formatQuantity(quantity: number) {
  if (quantity >= 100) return Math.round(quantity).toLocaleString("en-US");
  if (quantity >= 1) return quantity.toLocaleString("en-US", { maximumFractionDigits: 2 });
  return quantity.toLocaleString("en-US", { maximumSignificantDigits: 3 });
}

// Units ending in "s" are plural ("rows", "connections") except these.
const INVARIANT_UNITS = new Set(["CPU ms", "GB-s"]);

export function formatUsage(line: Pick<CostLineItem, "quantity" | "unit">) {
  const unit = line.quantity === 1 && !INVARIANT_UNITS.has(line.unit) ? line.unit.replace(/s$/, "") : line.unit;
  return `${formatQuantity(line.quantity)} ${unit}`;
}
