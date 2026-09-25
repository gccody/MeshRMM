import { ChevronDown, LoaderCircle } from "lucide-react";
import { BASIS_DESCRIPTIONS, formatPreciseUsd, formatRate, formatUsage, formatUsd, monthLabel, type CostReport, type CostSummary } from "./costs";

const PROVIDERS = [
  { key: "cloudflare", label: "Cloudflare" },
  { key: "workos", label: "WorkOS" },
] as const;

export function CostBreakdown({ summary }: { summary: CostSummary }) {
  if (summary.line_items.length === 0) return <p className="cost-empty">No usage recorded for this period.</p>;
  return (
    <div className="cost-breakdown">
      {PROVIDERS.map(({ key, label }) => {
        const lines = summary.line_items.filter((line) => line.provider === key);
        if (lines.length === 0) return null;
        return (
          <section key={key} aria-label={`${label} costs`}>
            <h3>{label}<span>{formatUsd(key === "cloudflare" ? summary.cloudflare_usd : summary.workos_usd)}</span></h3>
            <table>
              <thead><tr><th scope="col">Item</th><th scope="col">Usage</th><th scope="col">Rate</th><th scope="col">Cost</th></tr></thead>
              <tbody>
                {lines.map((line) => (
                  <tr key={line.item}>
                    <th scope="row">{line.label}<span className={`cost-basis basis-${line.basis}`} title={BASIS_DESCRIPTIONS[line.basis]}>{line.basis}</span></th>
                    <td>{formatUsage(line)}</td>
                    <td>{formatRate(line)}</td>
                    <td>{formatPreciseUsd(line.cost_usd)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </section>
        );
      })}
    </div>
  );
}

// Expands the platform's own costs rather than a company's.
export const PLATFORM_COSTS = "platform";

type ProviderCostsProps = {
  costs: CostReport | null;
  error: string | null;
  isLoading: boolean;
  month: string;
  months: string[];
  onMonthChange: (month: string) => void;
  expanded: string | null;
  onToggle: (key: string) => void;
};

export function ProviderCosts({ costs, error, isLoading, month, months, onMonthChange, expanded, onToggle }: ProviderCostsProps) {
  const providerTotal = (provider: "cloudflare_usd" | "workos_usd") =>
    costs ? costs.platform[provider] + costs.companies.reduce((sum, company) => sum + company[provider], 0) : 0;
  return (
    <section className="cost-card" aria-labelledby="provider-costs-heading" aria-busy={isLoading}>
      <div className="cost-card-heading">
        <div>
          <h2 id="provider-costs-heading">Provider costs</h2>
          <p>
            Cloudflare and WorkOS usage per company at paid-plan list prices, before included allowances.
            {costs && ` ${costs.complete ? `All of ${monthLabel(costs.month)}` : `${costs.start_date} to ${costs.end_date} UTC`}; prices as of ${costs.pricing_as_of}.`}
          </p>
        </div>
        <label className="cost-month">Month
          <select value={month} onChange={(event) => onMonthChange(event.target.value)}>
            {months.map((option) => <option key={option} value={option}>{monthLabel(option)}</option>)}
          </select>
        </label>
      </div>
      {error ? <p className="cost-error" role="alert">{error}</p> : !costs ? <div className="company-empty"><LoaderCircle className="spin" /> Loading costs</div> : (
        <>
          <div className="cost-stats">
            <div><span>Total</span><strong>{formatUsd(costs.total_usd)}</strong></div>
            <div><span>Cloudflare</span><strong>{formatUsd(providerTotal("cloudflare_usd"))}</strong></div>
            <div><span>WorkOS</span><strong>{formatUsd(providerTotal("workos_usd"))}</strong></div>
            <div>
              <span>Platform overhead</span><strong>{formatUsd(costs.platform.total_usd)}</strong>
              <button className="company-cost" aria-expanded={expanded === PLATFORM_COSTS} onClick={() => onToggle(PLATFORM_COSTS)}>Details <ChevronDown size={13} /></button>
            </div>
          </div>
          {costs.warnings.map((warning) => <p className="cost-warning" key={warning}>{warning}</p>)}
          {expanded === PLATFORM_COSTS && <CostBreakdown summary={costs.platform} />}
        </>
      )}
    </section>
  );
}
