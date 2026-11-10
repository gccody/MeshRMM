import { LoaderCircle } from "lucide-react";

// Loads the next page of a list: `remaining` more match than are loaded,
// and a page holds up to `pageSize`.
export function ShowMore({ remaining, pageSize, loading, disabled, onShowMore }: {
  remaining: number;
  pageSize: number;
  loading: boolean;
  disabled?: boolean;
  onShowMore: () => void;
}) {
  if (remaining <= 0) return null;
  return (
    <button type="button" className="secondary-button" disabled={loading || disabled} onClick={onShowMore}>
      {loading && <LoaderCircle size={15} className="spin" />} Show {Math.min(pageSize, remaining)} more
    </button>
  );
}
