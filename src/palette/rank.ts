const SEPARATORS = new Set(["_", ".", " ", "-", "/"]);
const isLower = (c: string) => c !== c.toUpperCase();
const isUpper = (c: string) => c !== c.toLowerCase();

/** Indexes where a word starts: 0, after `_ . space - /`, or at a lower→upper change. */
function wordStarts(text: string): number[] {
  const out: number[] = [];
  for (let i = 0; i < text.length; i++) {
    const prev = text[i - 1];
    if (SEPARATORS.has(text[i])) continue;
    if (i === 0 || SEPARATORS.has(prev) || (isLower(prev) && isUpper(text[i]))) out.push(i);
  }
  return out;
}

const isSubsequence = (q: string, text: string) => {
  let i = 0;
  for (const c of text) if (c === q[i] && ++i === q.length) return true;
  return q.length === 0;
};

/**
 * Match quality of `query` in `text`; `null` when it doesn't match. Higher is better. Tiers: exact
 * 400, prefix 300, word start 200 (a prefix of a word, or an acronym of word initials), substring
 * 100, subsequence 10. Within a tier, shorter text ranks higher.
 */
export function rank(query: string, text: string): number | null {
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  let tier: number;
  if (t === q) tier = 400;
  else if (t.startsWith(q)) tier = 300;
  else {
    const starts = wordStarts(text);
    if (starts.some((i) => t.startsWith(q, i)) || isSubsequence(q, starts.map((i) => t[i]).join(""))) tier = 200;
    else if (t.includes(q)) tier = 100;
    else if (isSubsequence(q, t)) tier = 10;
    else return null;
  }
  return tier * 1000 - text.length;
}
