import * as monaco from 'monaco-editor';
import { loader } from '@monaco-editor/react';

loader.config({ monaco });

const tsLang = (monaco.languages as any).typescript;
if (tsLang?.typescriptDefaults) {
  tsLang.typescriptDefaults.setCompilerOptions({
    target: tsLang.ScriptTarget?.ESNext ?? 99,
    allowNonTsExtensions: true,
    moduleResolution: tsLang.ModuleResolutionKind?.NodeJs ?? 2,
    jsx: tsLang.JsxEmit?.ReactJSX ?? 4,
    esModuleInterop: true,
  });
}

monaco.editor.defineTheme('kuda-dark', {
  base: 'vs-dark',
  inherit: true,
  rules: [
    { token: 'comment', foreground: '64748b', fontStyle: 'italic' },
    { token: 'keyword', foreground: '7dd3fc' },
    { token: 'string', foreground: '86efac' },
    { token: 'number', foreground: 'fcd34d' },
    { token: 'type.identifier', foreground: '67e8f9' },
    { token: 'function', foreground: '38bdf8' },
    { token: 'variable', foreground: 'e6edf3' },
    { token: 'constant', foreground: 'fbbf24' },
    { token: 'tag', foreground: 'f9a8d4' },
    { token: 'attribute.name', foreground: '93c5fd' },
    { token: 'delimiter', foreground: '94a3b8' },
  ],
  colors: {
    'editor.background': '#161a21',
    'editor.foreground': '#e6edf3',
    'editor.lineHighlightBackground': '#1c212880',
    'editorLineNumber.foreground': '#7d8b9c',
    'editorLineNumber.activeForeground': '#e6edf3',
    'editorCursor.foreground': '#38bdf8',
    'editor.selectionBackground': '#38bdf840',
    'editor.selectionHighlightBackground': '#38bdf825',
    'editorIndentGuide.background': '#232932',
    'editorIndentGuide.activeBackground': '#38bdf84d',
    'editorWidget.background': '#1c2128',
    'editorWidget.border': '#2a313b',
    'input.background': '#1c2128',
    'list.activeSelectionBackground': '#38bdf830',
    'list.inactiveSelectionBackground': '#38bdf81a',
    'list.hoverBackground': '#23293280',
    'scrollbarSlider.background': '#3d4a5c40',
    'scrollbarSlider.hoverBackground': '#38bdf855',
    'scrollbarSlider.activeBackground': '#38bdf87a',
    'minimap.background': '#161a21',
  },
});

loader.config({ monaco });

export { monaco };

export function languageForPath(path: string): string {
  const name = path.split('/').pop() ?? '';
  const ext = name.includes('.') ? name.split('.').pop()!.toLowerCase() : '';
  const lower = name.toLowerCase();

  if (lower === 'cargo.toml' || lower === 'cargo.lock' || ext === 'toml') return 'ini';
  if (lower === 'dockerfile') return 'dockerfile';

  const map: Record<string, string> = {
    rs: 'rust',
    ts: 'typescript',
    tsx: 'typescript',
    js: 'javascript',
    jsx: 'javascript',
    mjs: 'javascript',
    cjs: 'javascript',
    json: 'json',
    css: 'css',
    scss: 'scss',
    less: 'less',
    html: 'html',
    htm: 'html',
    md: 'markdown',
    markdown: 'markdown',
    py: 'python',
    sh: 'shell',
    bash: 'shell',
    zsh: 'shell',
    yml: 'yaml',
    yaml: 'yaml',
    xml: 'xml',
    sql: 'sql',
    go: 'go',
    c: 'c',
    h: 'c',
    cpp: 'cpp',
    hpp: 'cpp',
    java: 'java',
    kt: 'kotlin',
    swift: 'swift',
    rb: 'ruby',
    php: 'php',
    lua: 'lua',
    ini: 'ini',
    conf: 'ini',
    txt: 'plaintext',
  };
  return map[ext] ?? 'plaintext';
}
