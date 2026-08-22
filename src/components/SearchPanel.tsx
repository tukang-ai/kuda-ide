import React, { useState, useMemo } from 'react';
import {
  Search,
  Replace,
  ChevronRight,
  ChevronDown,
  X,
  FileText,
  Globe,
  SlidersHorizontal,
  RefreshCw,
} from 'lucide-react';
import * as ipc from '../lib/ipc';
import { useWorkspace } from '../store/workspace';
import type { SearchMatch } from '../types';

export const SearchPanel: React.FC = () => {
  const openFile = useWorkspace((s) => s.openFile);
  const activePath = useWorkspace((s) => s.activePath);
  const projectRoot = useWorkspace((s) => s.projectRoot);

  const [pattern, setPattern] = useState('');
  const [replacePattern, setReplacePattern] = useState('');
  const [isReplaceOpen, setIsReplaceOpen] = useState(false);
  const [isFiltersOpen, setIsFiltersOpen] = useState(false);

  // Scope: 'workspace' | 'specific_file' | 'current_file'
  const [searchScope, setSearchScope] = useState<'workspace' | 'specific_file' | 'current_file'>('workspace');

  // Options
  const [caseSensitive, setCaseSensitive] = useState(true);
  const [wholeWord, setWholeWord] = useState(false);
  const [isRegex, setIsRegex] = useState(false);

  // Filters
  const [includePattern, setIncludePattern] = useState('');
  const [excludePattern, setExcludePattern] = useState('');

  // Results & UI State
  const [rawResults, setRawResults] = useState<SearchMatch[] | null>(null);
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);
  const [collapsedFiles, setCollapsedFiles] = useState<Record<string, boolean>>({});

  const runReplace = async () => {
    const q = pattern.trim();
    if (!q || !rawResults || rawResults.length === 0) return;
    // Only rewrite the files the user actually SEES in the filtered result
    // list — scope/include/exclude filters used to be ignored and every
    // matching file in the workspace got modified.
    const targetFiles = Array.from(
      new Set((filteredResults ?? []).map((m) => m.file_path)),
    );
    if (targetFiles.length === 0) {
      setSearchError('No files match the current filters — nothing replaced.');
      return;
    }
    setSearching(true);
    setSearchError(null);
    try {
      let finalRegex = isRegex ? q : escapeRegex(q);
      if (wholeWord) {
        finalRegex = `\\b${finalRegex}\\b`;
      }
      const result = await ipc.searchReplace(
        {
          pattern: finalRegex,
          is_regex: true,
          case_sensitive: caseSensitive,
          max_results: 1000,
          // Literal search mode must insert `$100` verbatim, not expand it as
          // a regex template (which silently deleted matches).
          replacement_is_literal: !isRegex,
        },
        replacePattern,
        targetFiles,
      );
      setRawResults(null);
      setReplacePattern('');
      setIsReplaceOpen(false);
      setSearchError(
        `Replaced ${result.matches_replaced} occurrence(s) across ${result.files_changed} file(s).`,
      );
    } catch (err) {
      setSearchError(String(err));
    } finally {
      setSearching(false);
    }
  };

  const runSearch = async () => {
    const q = pattern.trim();
    if (!q) {
      setRawResults(null);
      return;
    }
    setSearching(true);
    setSearchError(null);
    try {
      let finalRegex = isRegex ? q : escapeRegex(q);
      if (wholeWord) {
        finalRegex = `\\b${finalRegex}\\b`;
      }
      const matches = await ipc.searchCode({
        pattern: finalRegex,
        is_regex: true,
        case_sensitive: caseSensitive,
        max_results: 1000,
      });
      setRawResults(matches);
    } catch (err) {
      setSearchError(String(err));
      setRawResults(null);
    } finally {
      setSearching(false);
    }
  };

  // Filter results by Scope (Workspace vs Current File) and Include/Exclude patterns
  const filteredResults = useMemo(() => {
    if (!rawResults) return null;

    return rawResults.filter((match) => {
      // 1. Scope filter
      if (searchScope === 'current_file') {
        if (!activePath) return false;
        if (match.file_path !== activePath && !match.file_path.endsWith(activePath)) {
          return false;
        }
      }

      const relPath = projectRoot && match.file_path.startsWith(projectRoot)
        ? match.file_path.slice(projectRoot.length).replace(/^[/\\]+/, '')
        : match.file_path;

      // 2. Include pattern filter
      if (includePattern.trim()) {
        const includes = includePattern.split(',').map((s) => s.trim()).filter(Boolean);
        const matchInc = includes.some((pat) => matchPathPattern(relPath, pat));
        if (!matchInc) return false;
      }

      // 3. Exclude pattern filter
      if (excludePattern.trim()) {
        const excludes = excludePattern.split(',').map((s) => s.trim()).filter(Boolean);
        const matchExc = excludes.some((pat) => matchPathPattern(relPath, pat));
        if (matchExc) return false;
      }

      return true;
    });
  }, [rawResults, searchScope, activePath, includePattern, excludePattern, projectRoot]);

  // Group by file
  const grouped = useMemo(() => {
    if (!filteredResults) return {};
    return filteredResults.reduce<Record<string, SearchMatch[]>>((acc, m) => {
      (acc[m.file_path] ??= []).push(m);
      return acc;
    }, {});
  }, [filteredResults]);

  const totalMatches = filteredResults?.length ?? 0;
  const totalFiles = Object.keys(grouped).length;

  const toggleFileCollapse = (filePath: string) => {
    setCollapsedFiles((prev) => ({ ...prev, [filePath]: !prev[filePath] }));
  };

  const collapseAll = () => {
    const all: Record<string, boolean> = {};
    Object.keys(grouped).forEach((fp) => {
      all[fp] = true;
    });
    setCollapsedFiles(all);
  };

  const expandAll = () => {
    setCollapsedFiles({});
  };

  const activeFileName = activePath ? activePath.split('/').pop() : null;

  return (
    <div className="search-panel-container">
      {/* Header Bar */}
      <div className="search-header-bar">
        <span className="search-header-title">
          <Search size={13} />
          Search
        </span>
        {totalMatches > 0 && (
          <div className="search-opt-group">
            <button className="search-action-icon-btn" onClick={collapseAll} title="Collapse All">
              <ChevronRight size={14} />
            </button>
            <button className="search-action-icon-btn" onClick={expandAll} title="Expand All">
              <ChevronDown size={14} />
            </button>
          </div>
        )}
      </div>

      {/* Scope Selector: Workspace vs Specific File vs Current File */}
      <div className="search-scope-bar">
        <button
          className={`search-scope-pill ${searchScope === 'workspace' ? 'active' : ''}`}
          onClick={() => setSearchScope('workspace')}
          title="Search across all files in project workspace"
        >
          <Globe size={12} />
          Workspace
        </button>
        <button
          className={`search-scope-pill ${searchScope === 'specific_file' ? 'active' : ''}`}
          onClick={() => {
            setSearchScope('specific_file');
            setIsFiltersOpen(true);
          }}
          title="Search inside specific file name or pattern (e.g. *.py, DOCUMENTATION.md)"
        >
          <SlidersHorizontal size={12} />
          Specific File
        </button>
        <button
          className={`search-scope-pill ${searchScope === 'current_file' ? 'active' : ''}`}
          onClick={() => setSearchScope('current_file')}
          title="Search exclusively in current active file"
          disabled={!activePath}
        >
          <FileText size={12} />
          Current Tab
        </button>
      </div>

      {/* Current File Banner */}
      {searchScope === 'current_file' && activeFileName && (
        <div className="search-active-file-indicator">
          <span>Targeting tab: <strong>{activeFileName}</strong></span>
          <button
            className="search-action-icon-btn"
            onClick={() => setSearchScope('workspace')}
            title="Switch to Workspace Search"
          >
            <X size={12} />
          </button>
        </div>
      )}

      {/* Main Search Controls */}
      <div className="search-controls-box">
        {/* Prominent Target File Field when Specific File scope is active */}
        {searchScope === 'specific_file' && (
          <div className="search-input-wrapper">
            <SlidersHorizontal size={13} className="search-input-left-icon" />
            <input
              className="search-input-field"
              placeholder="In file / pattern (e.g. *.py, train.py, DOCUMENTATION.md)"
              value={includePattern}
              onChange={(e) => setIncludePattern(e.target.value)}
              onKeyDown={(e) => e.key === 'Enter' && runSearch()}
              autoFocus
            />
            {includePattern && (
              <div className="search-input-right-actions">
                <button
                  className="search-action-icon-btn"
                  onClick={() => setIncludePattern('')}
                  title="Clear file pattern"
                >
                  <X size={13} />
                </button>
              </div>
            )}
          </div>
        )}

        {/* Search Field */}
        <div className="search-input-wrapper">
          <Search size={13} className="search-input-left-icon" />
          <input
            className="search-input-field"
            placeholder={
              searchScope === 'current_file'
                ? `Search query in ${activeFileName}…`
                : searchScope === 'specific_file'
                ? `Search query (e.g. ayam) in ${includePattern || 'target file'}…`
                : "Search query in workspace…"
            }
            value={pattern}
            onChange={(e) => setPattern(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && runSearch()}
          />
          <div className="search-input-right-actions">
            {pattern && (
              <button
                className="search-action-icon-btn"
                onClick={() => {
                  setPattern('');
                  setRawResults(null);
                }}
                title="Clear input"
              >
                <X size={13} />
              </button>
            )}
            <button
              className="search-action-icon-btn"
              onClick={runSearch}
              disabled={searching}
              title="Execute Search"
            >
              <RefreshCw size={13} className={searching ? 'pulse-status' : ''} />
            </button>
          </div>
        </div>


        {/* Replace Field (Expandable) */}
        {isReplaceOpen && (
          <div className="search-input-wrapper">
            <Replace size={13} className="search-input-left-icon" />
            <input
              className="search-input-field"
              placeholder="Replace string…"
              value={replacePattern}
              onChange={(e) => setReplacePattern(e.target.value)}
              onKeyDown={(e) => e.key === 'Enter' && runReplace()}
            />
            <div className="search-input-right-actions">
              <button
                className="search-action-icon-btn replace-all-btn"
                onClick={runReplace}
                disabled={searching || !pattern.trim() || !rawResults || rawResults.length === 0}
                title="Replace all occurrences across matching files"
              >
                <RefreshCw size={13} className={searching ? 'pulse-status' : ''} />
                Replace All
              </button>
            </div>
          </div>
        )}

        {/* Option Toolbar */}
        <div className="search-opt-toolbar">
          <div className="search-opt-group">
            <button
              className={`search-opt-btn ${caseSensitive ? 'active' : ''}`}
              onClick={() => setCaseSensitive(!caseSensitive)}
              title="Match Case (Aa)"
            >
              Aa
            </button>
            <button
              className={`search-opt-btn ${wholeWord ? 'active' : ''}`}
              onClick={() => setWholeWord(!wholeWord)}
              title="Match Whole Word (\b)"
            >
              \b
            </button>
            <button
              className={`search-opt-btn ${isRegex ? 'active' : ''}`}
              onClick={() => setIsRegex(!isRegex)}
              title="Use Regular Expression (.*)"
            >
              .*
            </button>
          </div>

          <div className="search-opt-group">
            <span
              className={`search-toggle-link ${isReplaceOpen ? 'active' : ''}`}
              onClick={() => setIsReplaceOpen(!isReplaceOpen)}
              title="Toggle Replace"
            >
              <Replace size={11} />
              Replace
            </span>
            <span
              className={`search-toggle-link ${isFiltersOpen ? 'active' : ''}`}
              onClick={() => setIsFiltersOpen(!isFiltersOpen)}
              title="Toggle File Filters"
            >
              <SlidersHorizontal size={11} />
              Filters
            </span>
          </div>
        </div>
      </div>

      {/* File Filters Accordion (Includes / Excludes) */}
      {isFiltersOpen && (
        <div className="search-filters-panel">
          <div className="search-filter-row">
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
              <span className="search-filter-label">files to include</span>
              <div className="search-chip-group">
                {['.py', '.ts', '.tsx', '.rs', '.json', '.md'].map((chip) => (
                  <span
                    key={chip}
                    className={`search-preset-chip ${includePattern.trim() === chip ? 'active' : ''}`}
                    onClick={() => setIncludePattern(includePattern.trim() === chip ? '' : chip)}
                    title={`Filter by ${chip} files`}
                  >
                    {chip}
                  </span>
                ))}
              </div>
            </div>
            <input
              className="search-filter-input"
              placeholder="e.g. .py, *.tsx, src/components/*, main.py"
              value={includePattern}
              onChange={(e) => setIncludePattern(e.target.value)}
            />
          </div>
          <div className="search-filter-row">
            <span className="search-filter-label">files to exclude</span>
            <input
              className="search-filter-input"
              placeholder="e.g. node_modules, dist, *.test.ts"
              value={excludePattern}
              onChange={(e) => setExcludePattern(e.target.value)}
            />
          </div>
        </div>
      )}


      {/* Summary Banner */}
      {filteredResults !== null && (
        <div className="search-summary-banner">
          <span>
            {searching
              ? 'Searching…'
              : `${totalMatches} result${totalMatches === 1 ? '' : 's'} in ${totalFiles} file${totalFiles === 1 ? '' : 's'}`}
          </span>
        </div>
      )}

      {/* Results Tree */}
      <div className="search-results-tree">
        {searchError && <div className="agent-error">{searchError}</div>}

        {filteredResults !== null && totalMatches === 0 && !searchError && (
          <div className="search-empty-box">
            <span>No results found matching query.</span>
          </div>
        )}

        {Object.entries(grouped).map(([filePath, matches]) => {
          const fileName = filePath.split('/').pop() ?? filePath;
          const dirPath = projectRoot && filePath.startsWith(projectRoot)
            ? filePath.slice(projectRoot.length).replace(/^[/\\]+/, '')
            : filePath;

          const isCollapsed = collapsedFiles[filePath] ?? false;

          return (
            <div key={filePath} className="search-file-card">
              <div className="search-file-header" onClick={() => toggleFileCollapse(filePath)}>
                {isCollapsed ? <ChevronRight size={13} /> : <ChevronDown size={13} />}
                <span className="search-file-name-text">{fileName}</span>
                <span className="search-file-dir-path" title={filePath}>{dirPath}</span>
                <span className="search-file-badge">{matches.length}</span>
              </div>

              {!isCollapsed && (
                <div className="search-matches-list">
                  {matches.map((m, idx) => (
                    <div
                      key={`${filePath}:${m.line_number}:${idx}`}
                      className="search-match-row"
                      onClick={() => openFile(m.file_path, m.line_number)}
                    >
                      <span className="search-match-num">{m.line_number}</span>
                      <div className="search-match-text">
                        {renderMatchSnippet(
                          m.line_content,
                          pattern,
                          isRegex,
                          caseSensitive,
                          wholeWord
                        )}
                      </div>
                    </div>
                  ))}
                </div>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
};

// Helper: Escape Regex characters
function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

// Helper: Flexible wildcard/path pattern matching
function matchPathPattern(path: string, pattern: string): boolean {
  const rawP = pattern.trim();
  if (!rawP) return true;

  const target = path.toLowerCase();
  const fileName = target.split('/').pop() || target;

  // Support multiple comma-separated patterns
  const patterns = rawP.split(',').map((s) => s.trim().toLowerCase()).filter(Boolean);
  if (patterns.length === 0) return true;

  return patterns.some((p) => {
    // Extension pattern like ".py"
    if (p.startsWith('.')) {
      return fileName.endsWith(p) || target.endsWith(p);
    }
    // Wildcard extension like "*.py"
    if (p.startsWith('*.')) {
      const ext = p.slice(1);
      return fileName.endsWith(ext) || target.endsWith(ext);
    }
    // General wildcard like "*py"
    if (p.startsWith('*')) {
      const sub = p.slice(1);
      return target.endsWith(sub) || fileName.endsWith(sub);
    }
    // Substring in file name or relative path
    return target.includes(p) || fileName.includes(p);
  });
}


// Helper: Highlight matching substring in line text
function renderMatchSnippet(
  lineContent: string,
  searchPattern: string,
  isRegex: boolean,
  caseSensitive: boolean,
  wholeWord: boolean
): React.ReactNode {
  const trimmed = lineContent.trim();
  if (!trimmed || !searchPattern.trim()) {
    return trimmed || ' ';
  }

  try {
    let pat = isRegex ? searchPattern : escapeRegex(searchPattern);
    if (wholeWord) pat = `\\b${pat}\\b`;
    const regex = new RegExp(`(${pat})`, caseSensitive ? 'g' : 'gi');
    const parts = trimmed.split(regex);

    return parts.map((part, index) => {
      const isMatch = regex.test(part);
      regex.lastIndex = 0;
      if (isMatch) {
        return (
          <mark key={index} className="search-match-highlight">
            {part}
          </mark>
        );
      }
      return <span key={index}>{part}</span>;
    });
  } catch {
    return trimmed;
  }
}

