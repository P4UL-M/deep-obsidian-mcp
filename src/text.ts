const STOPWORDS = new Set([
  "a",
  "an",
  "and",
  "are",
  "as",
  "at",
  "be",
  "by",
  "for",
  "from",
  "how",
  "in",
  "into",
  "is",
  "it",
  "of",
  "on",
  "or",
  "that",
  "the",
  "this",
  "to",
  "with",
]);

export function tokenize(text: string): string[] {
  const words = text.toLowerCase().match(/[a-z0-9][a-z0-9_-]*/g) ?? [];
  return words.filter((word) => word.length > 1 && !STOPWORDS.has(word));
}

export function countTerms(text: string): Record<string, number> {
  const counts: Record<string, number> = {};
  for (const token of tokenize(text)) {
    counts[token] = (counts[token] ?? 0) + 1;
  }
  return counts;
}

export function tokenCount(termCounts: Record<string, number>): number {
  let total = 0;
  for (const value of Object.values(termCounts)) {
    total += value;
  }
  return total;
}

export function vectorNorm(termCounts: Record<string, number>): number {
  let sum = 0;
  for (const value of Object.values(termCounts)) {
    sum += value * value;
  }
  return Math.sqrt(sum);
}

export function frontmatterTitle(content: string): string | null {
  if (!content.startsWith("---\n")) {
    return null;
  }

  const lines = content.split(/\r?\n/);
  for (let index = 1; index < lines.length; index += 1) {
    const line = lines[index];
    if (line === "---") {
      break;
    }
    if (line.toLowerCase().startsWith("title:")) {
      return line.slice("title:".length).trim().replace(/^['"]|['"]$/g, "");
    }
  }

  return null;
}

export function headingTitle(content: string): string | null {
  const lines = content.split(/\r?\n/);
  for (const line of lines) {
    if (line.startsWith("# ")) {
      return line.slice(2).trim();
    }
  }
  return null;
}

export function noteTitle(pathStem: string, content: string): string {
  return frontmatterTitle(content) ?? headingTitle(content) ?? pathStem;
}

export function extractWikiLinks(content: string): string[] {
  const matches = content.match(/\[\[[^[\]]+\]\]/g) ?? [];
  return matches.map((match) => match.slice(2, -2).split("|", 1)[0].trim()).filter(Boolean);
}

export function normalizeHeadingSlug(text: string): string {
  return text
    .trim()
    .toLowerCase()
    .replace(/[`*_~[\](){}<>#!?.,:;'"\\/]+/g, "")
    .replace(/\s+/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "");
}

export interface HeadingSection {
  level: number;
  title: string;
  slug: string;
  startLine: number;
  endLine: number;
  text: string;
}

export function extractHeadingSections(content: string): HeadingSection[] {
  const lines = content.split(/\r?\n/);
  const headings: Array<{ level: number; title: string; slug: string; line: number }> = [];

  for (let index = 0; index < lines.length; index += 1) {
    const match = lines[index].match(/^(#{1,6})\s+(.*)$/);
    if (!match) {
      continue;
    }
    const title = match[2].trim();
    headings.push({
      level: match[1].length,
      title,
      slug: normalizeHeadingSlug(title),
      line: index + 1,
    });
  }

  return headings.map((heading, index) => {
    let endLine = lines.length;
    for (let nextIndex = index + 1; nextIndex < headings.length; nextIndex += 1) {
      if (headings[nextIndex].level <= heading.level) {
        endLine = headings[nextIndex].line - 1;
        break;
      }
    }
    return {
      level: heading.level,
      title: heading.title,
      slug: heading.slug,
      startLine: heading.line,
      endLine,
      text: lines.slice(heading.line - 1, endLine).join("\n"),
    };
  });
}

export interface BlockSection {
  id: string;
  startLine: number;
  endLine: number;
  text: string;
}

export function extractBlockSections(content: string): BlockSection[] {
  const lines = content.split(/\r?\n/);
  const blocks: BlockSection[] = [];

  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    const inlineMatch = line.match(/^(.*?)(?:\s+)?\^([A-Za-z0-9-]+)\s*$/);
    if (!inlineMatch) {
      continue;
    }

    const id = inlineMatch[2];
    const inlineText = inlineMatch[1].trim();
    if (inlineText) {
      blocks.push({
        id,
        startLine: index + 1,
        endLine: index + 1,
        text: inlineText,
      });
      continue;
    }

    let startLine = index;
    while (startLine > 0) {
      const previous = lines[startLine - 1];
      if (!previous.trim() || /^(#{1,6})\s+/.test(previous)) {
        break;
      }
      startLine -= 1;
    }

    const text = lines.slice(startLine, index).join("\n").trim();
    blocks.push({
      id,
      startLine: startLine + 1,
      endLine: index + 1,
      text,
    });
  }

  return blocks;
}
