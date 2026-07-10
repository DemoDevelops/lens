// xs.map(f) mentioned in a comment, not a real call site.
function realMatches(xs: number[], f: (n: number) => number) {
  const mapped = xs.map(f);
  const filtered = xs.filter(f);
  const s = "xs.map(f) inside a string, not a call";
  return mapped.length + filtered.length + s.length;
}
