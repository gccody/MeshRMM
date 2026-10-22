import { type JobCounts, type JobKind, OUTCOMES, type Outcome, outcomeLabel, outcomeTone, progressLabel, progressSegments } from "./model";

// How far a job got: a bar of its devices' results, colored by outcome.
// Drawn in SVG, since the website's pages carry no inline styles.
export function JobProgressBar({ counts }: { counts: JobCounts }) {
  const label = progressLabel(counts);
  return (
    <svg className="job-progress" role="img" aria-label={label}>
      <title>{label}</title>
      <rect className="job-progress-track" width="100%" height="6" rx="3" />
      {progressSegments(counts).map((segment) => (
        <rect key={segment.tone} className={`job-progress-${segment.tone}`} x={`${(segment.start * 100).toFixed(2)}%`} width={`${(segment.fraction * 100).toFixed(2)}%`} height="6" />
      ))}
    </svg>
  );
}

// The job's non-zero outcomes as chips; with `onSelect`, chips filter the
// devices, and `selected` is the chosen one.
export function OutcomeChips({ counts, kind, selected = null, onSelect }: {
  counts: JobCounts;
  // Wake jobs word some outcomes their own way.
  kind?: JobKind;
  selected?: Outcome | null;
  onSelect?: (outcome: Outcome | null) => void;
}) {
  const shown = OUTCOMES.filter((outcome) => counts[outcome] > 0);
  if (!onSelect) {
    return (
      <ul className="outcome-chips">
        {shown.map((outcome) => <li key={outcome} className={`outcome-chip outcome-${outcomeTone(outcome)}`}>{outcomeLabel(outcome, kind)}<em>{counts[outcome]}</em></li>)}
      </ul>
    );
  }
  return (
    <div className="outcome-chips" role="group" aria-label="Show devices">
      <button type="button" className="outcome-chip" aria-pressed={selected === null} onClick={() => onSelect(null)}>All<em>{counts.total}</em></button>
      {shown.map((outcome) => (
        <button key={outcome} type="button" className={`outcome-chip outcome-${outcomeTone(outcome)}`} aria-pressed={selected === outcome} onClick={() => onSelect(selected === outcome ? null : outcome)}>
          {outcomeLabel(outcome, kind)}<em>{counts[outcome]}</em>
        </button>
      ))}
    </div>
  );
}

// A device's outcome as a pill.
export function OutcomePill({ outcome, kind }: { outcome: Outcome; kind?: JobKind }) {
  return <span className={`run-pill run-pill-${outcomeTone(outcome)}`}>{outcomeLabel(outcome, kind)}</span>;
}
