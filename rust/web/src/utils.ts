export const interpolate = (x: number, x1: number, y1: number, x2: number, y2: number) => {
  return y1 + (y2 - y1) * ((x - x1) / (x2 - x1))
}

export const formatLocalDateTime = (value: string): string | undefined => {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return undefined;
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(date);
};

export const clamp = (x: number, x1: number, x2: number) => {
  return Math.max(x1, Math.min(x, x2));
};

export const roundTo = (x: number, every_x: number = 10) => {
  return Math.round(x / every_x) * every_x;
};
