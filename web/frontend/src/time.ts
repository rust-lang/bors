export function formatDurationMs(ms: number): string {
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  const days = Math.floor(totalSeconds / 86400);
  const hours = Math.floor((totalSeconds % 86400) / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);

  const parts: string[] = [];
  if (days > 0) {
    parts.push(`${days}d`);
  }
  if (hours > 0) {
    parts.push(`${hours}h`);
  }
  if (days === 0 && minutes > 0) {
    parts.push(`${minutes}m`);
  }

  return parts.length > 0 ? parts.join(" ") : "<1m";
}

export function formatRemainingMs(remainingMs: number): string {
  if (remainingMs >= 0) {
    return `${formatDurationMs(remainingMs)} left`;
  }
  return `${formatDurationMs(-remainingMs)} overdue`;
}

export function formatTimestamp(ms: number): string {
  return new Date(ms).toLocaleString();
}

export function calculateProgressPct(
  elapsedMs: number,
  averageDurationMs: number,
): number {
  if (averageDurationMs <= 0) {
    return 0;
  }
  return Math.min(100, Math.max(0, (elapsedMs / averageDurationMs) * 100));
}
