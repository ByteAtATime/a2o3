const slugEscapes: Record<string, string> = {
  "*s*": "/",
  "*d*": ".",
  "*a*": "&",
  "*b*": "!",
  "*e*": ":",
  "*f*": "?",
  "*h*": "#",
  "*i*": "'",
  "*k*": "(",
  "*l*": ")",
  "*m*": ",",
  "*n*": "-",
  "*q*": '"',
  "*t*": "+",
  "*u*": "_",
};

export const formatTag = (tag: string): string =>
  tag.replace(/\*[a-qs-z]\*/g, (code) => slugEscapes[code] ?? code);

export const tagPath = (tag: string): string =>
  `/tags/${encodeURIComponent(tag)}`;

export const parseTag = (tag: string): { label: string; href: string } => ({
  label: formatTag(tag),
  href: tagPath(tag),
});

export const cleanSummary = (summary: string): string =>
  summary
    .replace(/<br\s*\/?>/gi, "\n")
    .replace(/\s*<\/?[a-z][^>]*>\s*/gi, "\n")
    .replace(/[ \t]+\n/g, "\n")
    .trim();

const sentenceEnd = /\.(?=\s*["“'‘]?[A-Z0-9“‘])|[!?]["”'’]?(?=\s)/g;

export const paragraphs = (text: string): string[] =>
  text
    .split(/\n+/)
    .flatMap((line) => line.split(sentenceEnd))
    .map((paragraph) => paragraph.trim())
    .filter((paragraph) => paragraph !== "");
