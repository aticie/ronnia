export const useMinuteFormat = (seconds: number) => {
  let rounded = Math.round(seconds);

  if (seconds < 60) return `${rounded}s`;
  if (seconds % 60 === 0) return `${Math.round(seconds / 60)}m`;
  return `${Math.floor(rounded / 60)}m${rounded % 60}s`;
}
