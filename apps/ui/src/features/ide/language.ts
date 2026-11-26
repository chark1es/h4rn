const LANGUAGES: Record<string, string> = {
  ts: "TypeScript",
  tsx: "TypeScript React",
  js: "JavaScript",
  jsx: "JavaScript React",
  rs: "Rust",
  py: "Python",
  go: "Go",
  json: "JSON",
  md: "Markdown",
  css: "CSS",
  html: "HTML",
  yaml: "YAML",
  yml: "YAML",
  toml: "TOML",
};

/**
 * The status bar's language name for a file, from its extension.
 *
 * Language names read the same in every language, so they are not
 * translated. `null` means plain text, which the status bar words in the
 * active language.
 */
export function languageLabel(path: string | null | undefined): string | null {
  if (!path) return null;
  const extension = path.split(".").at(-1)?.toLowerCase();
  return (extension && LANGUAGES[extension]) || null;
}
