import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { openUrl } from '@tauri-apps/plugin-opener';
import { check as checkUpdater, type DownloadEvent } from '@tauri-apps/plugin-updater';
import { useEffect, useMemo, useRef, useState, type MouseEvent } from 'react';
import { Check, Copy, MoreHorizontal, RefreshCcw, Search, Settings, Star } from 'lucide-react';
import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';

interface Skill {
  id: string;
  name: string;
  sourceName: string;
  sourceUsage: string;
  sourceDescription: string;
  sourceMarkdown: string;
  sourceCommands: string[];
  aiBrief: string;
  aiDetail: string;
  path: string;
  definitionPath: string;
  installedAt?: number | null;
  firstSeenAt?: number | null;
  favorite?: boolean;
}

interface Platform {
  id: string;
  name: string;
  root: string;
  skills: Skill[];
}

interface SkillsSnapshot {
  scannedAt: number;
  aiSummarizedCount: number;
  aiPendingCount: number;
  platforms: Platform[];
}

interface AiSettingsStatus {
  hasKey: boolean;
  maskedKey?: string | null;
}

interface UpdateToast {
  currentVersion: string;
  latestVersion: string;
  releaseUrl?: string | null;
}

interface FlatSkill extends Skill {
  key: string;
  platformId: string;
  platformName: string;
  platformRoot: string;
  searchText: string;
}

interface ResummarizeQueueItem {
  key: string;
  platformId: string;
  skillId: string;
  skillName: string;
}

interface SkillContextMenuState {
  skillKey: string;
  x: number;
  y: number;
}

interface ScanProgressPayload {
  stage: string;
  message: string;
  newSkillsCount: number;
  summarizedCount: number;
  summarizeTotal: number;
  currentSkill?: string | null;
}

interface AiSummaryStreamPayload {
  platformId: string;
  skillId: string;
  detailMarkdown: string;
  done: boolean;
}

function normalizeAiSummaryStreamPayload(input: unknown): AiSummaryStreamPayload | null {
  if (input == null) {
    return null;
  }

  let raw: any = input;
  if (typeof raw === 'string') {
    try {
      raw = JSON.parse(raw);
    } catch {
      return null;
    }
  }
  if (typeof raw !== 'object') {
    return null;
  }

  const platformId = String(raw.platformId ?? raw.platform_id ?? '').trim();
  const skillId = String(raw.skillId ?? raw.skill_id ?? '').trim();
  const detailMarkdown = String(raw.detailMarkdown ?? raw.detail_markdown ?? '');
  const done = Boolean(raw.done);

  if (!platformId || !skillId) {
    return null;
  }
  return { platformId, skillId, detailMarkdown, done };
}

function getInvokeErrorMessage(err: unknown, fallback: string): string {
  if (typeof err === 'string' && err.trim()) {
    return err;
  }
  if (err instanceof Error && err.message.trim()) {
    return err.message;
  }
  if (err && typeof err === 'object' && 'message' in err) {
    const message = (err as { message?: unknown }).message;
    if (typeof message === 'string' && message.trim()) {
      return message;
    }
  }
  return fallback;
}

function toFriendlyUpdaterCheckMessage(err: unknown, fallback: string): string {
  const raw = getInvokeErrorMessage(err, fallback);
  const lower = raw.toLowerCase();
  if (
    raw.includes('没有可用正式更新') ||
    lower.includes('could not fetch a valid release json') ||
    lower.includes('valid release json') ||
    lower.includes('404') ||
    lower.includes('not found') ||
    lower.includes('latest.json')
  ) {
    return '没有可用正式更新。';
  }
  if (raw.includes('过于频繁') || lower.includes('429') || lower.includes('rate limit')) {
    return '更新服务暂时不可用，请稍后重试。';
  }
  return raw;
}

function toFriendlyAiMessage(message: string, fallback: string): string {
  const text = message.trim();
  const lower = text.toLowerCase();

  if (!text) {
    return fallback;
  }
  if (text.includes('AI 总结任务已停止')) {
    return '总结任务已停止。';
  }
  if (text.includes('请先在设置中填写 DeepSeek Key')) {
    return '请先输入并保存 DeepSeek Key。';
  }
  if (
    lower.includes('http 401') ||
    lower.includes('invalid api key') ||
    lower.includes('unauthorized')
  ) {
    return 'Key 无效，请检查后重新保存。';
  }
  if (
    lower.includes('http 402') ||
    lower.includes('insufficient') ||
    lower.includes('quota') ||
    lower.includes('余额')
  ) {
    return '账户额度不足，请检查 DeepSeek 余额。';
  }
  if (lower.includes('http 429') || lower.includes('rate limit')) {
    return '请求过于频繁，请稍后再试。';
  }
  if (lower.includes('timeout') || lower.includes('timed out')) {
    return '连接超时，请稍后重试。';
  }
  if (
    lower.includes('network') ||
    text.includes('请求失败') ||
    lower.includes('dns') ||
    lower.includes('connection')
  ) {
    return '网络连接失败，请检查网络后重试。';
  }
  if (text.includes('响应解析失败') || text.includes('格式解析失败') || text.includes('响应缺少')) {
    return '服务返回异常，请稍后再试。';
  }
  return fallback;
}

function truncateForSearch(input: string, maxChars: number): string {
  const text = input.trim();
  if (!text) {
    return '';
  }
  return text.length <= maxChars ? text : text.slice(0, maxChars);
}

function isSafeExternalHref(href: string): boolean {
  const lower = href.toLowerCase();
  return lower.startsWith('http://') || lower.startsWith('https://') || lower.startsWith('mailto:');
}

function updateDismissKey(version: string): string {
  return `skillsbox.update.dismissed.${version}`;
}

const RESUMMARIZE_SKILL_MIN_CONCURRENT = 2;
const RESUMMARIZE_SKILL_MAX_CONCURRENT = 10;

export function AppContent() {
  const [platforms, setPlatforms] = useState<Platform[]>([]);
  const [searchQuery, setSearchQuery] = useState('');
  const [selectedSkillKey, setSelectedSkillKey] = useState('');
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState('');
  const [manualRefreshing, setManualRefreshing] = useState(false);
  const [refreshButtonText, setRefreshButtonText] = useState('刷新');
  const [copiedKey, setCopiedKey] = useState('');
  const [showSettings, setShowSettings] = useState(false);
  const [aiSummarizedCount, setAiSummarizedCount] = useState(0);
  const [aiPendingCount, setAiPendingCount] = useState(0);
  const [apiKeyInput, setApiKeyInput] = useState('');
  const [hasApiKey, setHasApiKey] = useState(false);
  const [savedMaskedKey, setSavedMaskedKey] = useState('');
  const [showMaskedKey, setShowMaskedKey] = useState(false);
  const [savingKey, setSavingKey] = useState(false);
  const [testingKey, setTestingKey] = useState(false);
  const [summarizingAi, setSummarizingAi] = useState(false);
  const [confirmSummarizePending, setConfirmSummarizePending] = useState(false);
  const [resummarizingAllAi, setResummarizingAllAi] = useState(false);
  const [confirmResummarizeAll, setConfirmResummarizeAll] = useState(false);
  const [resummarizingAllProgressText, setResummarizingAllProgressText] = useState('');
  const [resummarizingAllCurrentSkill, setResummarizingAllCurrentSkill] = useState('');
  const [resummarizingSkillRunningKeys, setResummarizingSkillRunningKeys] = useState<string[]>([]);
  const [resummarizingSkillQueuedKeys, setResummarizingSkillQueuedKeys] = useState<string[]>([]);
  const [skillContextMenu, setSkillContextMenu] = useState<SkillContextMenuState | null>(null);
  const [summaryQueueProgressDone, setSummaryQueueProgressDone] = useState(0);
  const [summaryQueueProgressTotal, setSummaryQueueProgressTotal] = useState(0);
  const [stoppingAllSummaries, setStoppingAllSummaries] = useState(false);
  const [showSkillActionMenu, setShowSkillActionMenu] = useState(false);
  const [showOnboardingModal, setShowOnboardingModal] = useState(false);
  const [settingsMessage, setSettingsMessage] = useState('');
  const [appVersion, setAppVersion] = useState('');
  const [checkingUpdate, setCheckingUpdate] = useState(false);
  const [updatingApp, setUpdatingApp] = useState(false);
  const [updateProgressText, setUpdateProgressText] = useState('');
  const [updateMessage, setUpdateMessage] = useState('');
  const [updateToast, setUpdateToast] = useState<UpdateToast | null>(null);
  const [contentTab, setContentTab] = useState<'detail' | 'source'>('detail');
  const loadInFlightRef = useRef(false);
  const queuedForceRefreshRef = useRef(false);
  const manualRefreshActiveRef = useRef(false);
  const manualRefreshStartedAtRef = useRef(0);
  const copiedResetTimerRef = useRef<number | null>(null);
  const settingsMessageAutoClearTimerRef = useRef<number | null>(null);
  const updateMessageAutoClearTimerRef = useRef<number | null>(null);
  const summarizePendingConfirmResetTimerRef = useRef<number | null>(null);
  const resummarizeProgressResetTimerRef = useRef<number | null>(null);
  const resummarizeConfirmResetTimerRef = useRef<number | null>(null);
  const resummarizeSkillRunningRef = useRef<Set<string>>(new Set());
  const resummarizeSkillQueueRef = useRef<ResummarizeQueueItem[]>([]);
  const resummarizeSkillConcurrencyLimitRef = useRef(RESUMMARIZE_SKILL_MIN_CONCURRENT);
  const summarizePendingBatchKeysRef = useRef<Set<string>>(new Set());
  const resummarizeAllBatchKeysRef = useRef<Set<string>>(new Set());
  const resummarizeAllBatchTotalRef = useRef(0);
  const resummarizeAllBatchDoneRef = useRef(0);
  const resummarizeQueuePumpActiveRef = useRef(false);
  const summaryQueueProgressDoneRef = useRef(0);
  const summaryQueueProgressTotalRef = useRef(0);
  const skillActionMenuRef = useRef<HTMLDivElement | null>(null);
  const [debouncedSearchQuery, setDebouncedSearchQuery] = useState('');

  const markdownComponents = useMemo<Components>(
    () => ({
      a: ({ href, children, ...props }) => {
        const target = typeof href === 'string' ? href.trim() : '';
        if (!target) {
          return <span>{children}</span>;
        }

        if (!isSafeExternalHref(target)) {
          return (
            <span className="cursor-not-allowed text-zinc-400" title="已拦截不安全链接">
              {children}
            </span>
          );
        }

        const onClick = (event: MouseEvent<HTMLAnchorElement>) => {
          event.preventDefault();
          void openUrl(target).catch(() => {
            window.open(target, '_blank', 'noopener,noreferrer');
          });
        };

        return (
          <a href={target} rel="noreferrer noopener" target="_blank" onClick={onClick} {...props}>
            {children}
          </a>
        );
      },
      table: ({ children }) => (
        <div className="markdown-table-wrap">
          <table>{children}</table>
        </div>
      ),
    }),
    [],
  );
  const emptyAiHint = '请点击右上角设置按钮，配置好 API 后，点击“补全未总结”。';

  const allSkills = useMemo<FlatSkill[]>(
    () =>
      platforms.flatMap((platform) =>
        platform.skills.map((skill) => ({
          ...skill,
          key: `${platform.id}::${skill.id}`,
          platformId: platform.id,
          platformName: platform.name,
          platformRoot: platform.root,
          searchText:
            `${skill.name} ${skill.id} ${truncateForSearch(skill.aiBrief, 160)} ${truncateForSearch(skill.aiDetail, 1200)} ${truncateForSearch(skill.sourceUsage, 420)} ${truncateForSearch(skill.sourceDescription, 420)} ${truncateForSearch(skill.sourceMarkdown, 1200)} ${skill.path} ${platform.name}`.toLowerCase(),
        })),
      ),
    [platforms],
  );

  useEffect(() => {
    const timer = window.setTimeout(() => {
      setDebouncedSearchQuery(searchQuery);
    }, 220);
    return () => window.clearTimeout(timer);
  }, [searchQuery]);

  const filteredSkills = useMemo(() => {
    const q = debouncedSearchQuery.trim().toLowerCase();
    const rows = q ? allSkills.filter((skill) => skill.searchText.includes(q)) : allSkills.slice();

    rows.sort((a, b) => {
      const timeA = a.firstSeenAt ?? a.installedAt ?? 0;
      const timeB = b.firstSeenAt ?? b.installedAt ?? 0;
      if (timeA !== timeB) {
        return timeB - timeA;
      }
      return a.name.localeCompare(b.name, 'zh-Hans-CN');
    });
    return rows;
  }, [allSkills, debouncedSearchQuery]);

  const selectedSkill = useMemo(
    () => allSkills.find((skill) => skill.key === selectedSkillKey) ?? null,
    [allSkills, selectedSkillKey],
  );
  const hasSelectedAiBrief = Boolean(selectedSkill?.aiBrief?.trim());
  const hasSelectedAiDetail = Boolean(selectedSkill?.aiDetail?.trim());
  const summarizingRunningSkillKeys = useMemo(() => {
    const keys = new Set<string>();

    for (const resummarizingSkillKey of resummarizingSkillRunningKeys) {
      keys.add(resummarizingSkillKey);
    }

    return keys;
  }, [resummarizingSkillRunningKeys]);
  const summarizingQueuedSkillKeys = useMemo(() => {
    const keys = new Set<string>();
    for (const key of resummarizingSkillQueuedKeys) {
      if (!summarizingRunningSkillKeys.has(key)) {
        keys.add(key);
      }
    }
    return keys;
  }, [resummarizingSkillQueuedKeys, summarizingRunningSkillKeys]);
  const isSelectedSkillSummarizing = Boolean(
    selectedSkill &&
      (summarizingRunningSkillKeys.has(selectedSkill.key) ||
        summarizingQueuedSkillKeys.has(selectedSkill.key)),
  );
  const isSelectedSkillRunning = Boolean(
    selectedSkill && summarizingRunningSkillKeys.has(selectedSkill.key),
  );
  const isSelectedSkillQueued = Boolean(
    selectedSkill && !isSelectedSkillRunning && summarizingQueuedSkillKeys.has(selectedSkill.key),
  );
  const hasSummaryQueueActivity =
    resummarizingSkillRunningKeys.length > 0 || resummarizingSkillQueuedKeys.length > 0;
  const contextMenuSkill = useMemo(() => {
    if (!skillContextMenu) {
      return null;
    }
    return allSkills.find((skill) => skill.key === skillContextMenu.skillKey) ?? null;
  }, [allSkills, skillContextMenu]);
  const isContextMenuSkillSummarizing = Boolean(
    contextMenuSkill &&
      (summarizingRunningSkillKeys.has(contextMenuSkill.key) ||
        summarizingQueuedSkillKeys.has(contextMenuSkill.key)),
  );

  useEffect(() => {
    if (allSkills.length === 0) {
      setSelectedSkillKey('');
      return;
    }
    if (!selectedSkillKey || !allSkills.some((skill) => skill.key === selectedSkillKey)) {
      setSelectedSkillKey(allSkills[0].key);
    }
  }, [allSkills, selectedSkillKey]);

  useEffect(() => {
    setCopiedKey('');
    setContentTab('detail');
    setShowSkillActionMenu(false);
    setSkillContextMenu(null);
  }, [selectedSkillKey]);

  useEffect(() => {
    if (!showSkillActionMenu) {
      return;
    }
    const onMouseDown = (event: globalThis.MouseEvent) => {
      const root = skillActionMenuRef.current;
      if (!root) {
        return;
      }
      if (event.target instanceof Node && !root.contains(event.target)) {
        setShowSkillActionMenu(false);
      }
    };
    window.addEventListener('mousedown', onMouseDown);
    return () => {
      window.removeEventListener('mousedown', onMouseDown);
    };
  }, [showSkillActionMenu]);

  useEffect(() => {
    if (!skillContextMenu) {
      return;
    }
    const dismiss = () => setSkillContextMenu(null);
    const onMouseDown = () => dismiss();
    const onWheel = () => dismiss();
    const onResize = () => dismiss();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        dismiss();
      }
    };
    window.addEventListener('mousedown', onMouseDown);
    window.addEventListener('wheel', onWheel, { passive: true });
    window.addEventListener('resize', onResize);
    window.addEventListener('keydown', onKeyDown);
    return () => {
      window.removeEventListener('mousedown', onMouseDown);
      window.removeEventListener('wheel', onWheel);
      window.removeEventListener('resize', onResize);
      window.removeEventListener('keydown', onKeyDown);
    };
  }, [skillContextMenu]);

  useEffect(
    () => () => {
      if (copiedResetTimerRef.current) {
        window.clearTimeout(copiedResetTimerRef.current);
        copiedResetTimerRef.current = null;
      }
      if (settingsMessageAutoClearTimerRef.current) {
        window.clearTimeout(settingsMessageAutoClearTimerRef.current);
        settingsMessageAutoClearTimerRef.current = null;
      }
      if (updateMessageAutoClearTimerRef.current) {
        window.clearTimeout(updateMessageAutoClearTimerRef.current);
        updateMessageAutoClearTimerRef.current = null;
      }
      if (summarizePendingConfirmResetTimerRef.current) {
        window.clearTimeout(summarizePendingConfirmResetTimerRef.current);
        summarizePendingConfirmResetTimerRef.current = null;
      }
      if (resummarizeProgressResetTimerRef.current) {
        window.clearTimeout(resummarizeProgressResetTimerRef.current);
        resummarizeProgressResetTimerRef.current = null;
      }
      if (resummarizeConfirmResetTimerRef.current) {
        window.clearTimeout(resummarizeConfirmResetTimerRef.current);
        resummarizeConfirmResetTimerRef.current = null;
      }
    },
    [],
  );

  useEffect(() => {
    const onContextMenu = (event: globalThis.MouseEvent) => {
      event.preventDefault();
    };
    window.addEventListener('contextmenu', onContextMenu);
    return () => {
      window.removeEventListener('contextmenu', onContextMenu);
    };
  }, []);

  useEffect(() => {
    if (settingsMessageAutoClearTimerRef.current) {
      window.clearTimeout(settingsMessageAutoClearTimerRef.current);
      settingsMessageAutoClearTimerRef.current = null;
    }
    if (!settingsMessage.trim()) {
      return;
    }
    const current = settingsMessage;
    settingsMessageAutoClearTimerRef.current = window.setTimeout(() => {
      setSettingsMessage((value) => (value === current ? '' : value));
      settingsMessageAutoClearTimerRef.current = null;
    }, 5000);
  }, [settingsMessage]);

  useEffect(() => {
    if (updateMessageAutoClearTimerRef.current) {
      window.clearTimeout(updateMessageAutoClearTimerRef.current);
      updateMessageAutoClearTimerRef.current = null;
    }
    if (!updateMessage.trim()) {
      return;
    }
    const current = updateMessage;
    updateMessageAutoClearTimerRef.current = window.setTimeout(() => {
      setUpdateMessage((value) => (value === current ? '' : value));
      updateMessageAutoClearTimerRef.current = null;
    }, 5000);
  }, [updateMessage]);

  const applySnapshot = (snapshot: SkillsSnapshot) => {
    setPlatforms(snapshot.platforms);
    setAiSummarizedCount(snapshot.aiSummarizedCount ?? 0);
    setAiPendingCount(snapshot.aiPendingCount ?? 0);
  };

  const loadSkills = async (options?: { force?: boolean; initial?: boolean; manual?: boolean }) => {
    const force = options?.force ?? false;
    const initial = options?.initial ?? false;
    const manual = options?.manual ?? false;
    let refreshFailed = false;

    if (loadInFlightRef.current) {
      if (force) {
        queuedForceRefreshRef.current = true;
        if (manual) {
          manualRefreshActiveRef.current = true;
          setManualRefreshing(true);
          setRefreshButtonText('排队中...');
        }
      }
      return;
    }

    loadInFlightRef.current = true;
    if (initial) {
      setLoading(true);
    } else {
      setRefreshing(true);
    }
    setError('');
    try {
      if (manual) {
        manualRefreshActiveRef.current = true;
        manualRefreshStartedAtRef.current = Date.now();
        setManualRefreshing(true);
        setRefreshButtonText('准备刷新...');
        await new Promise<void>((resolve) => {
          window.requestAnimationFrame(() => resolve());
        });
        setRefreshButtonText('正在刷新...');
      }
      const command = force ? 'refresh_skills_with_auto_ai' : 'load_skills';
      const snapshot = await invoke<SkillsSnapshot>(command);
      applySnapshot(snapshot);
    } catch (err) {
      setError(getInvokeErrorMessage(err, '加载失败'));
      if (manual) {
        refreshFailed = true;
        setRefreshButtonText('刷新失败');
      }
    } finally {
      if (initial) {
        setLoading(false);
      } else {
        setRefreshing(false);
      }
      if (manual) {
        const elapsed = Date.now() - manualRefreshStartedAtRef.current;
        const remain = Math.max(0, 900 - elapsed);
        if (remain > 0) {
          await new Promise((resolve) => window.setTimeout(resolve, remain));
        }
        if (!refreshFailed) {
          setRefreshButtonText('刷新完成');
        }
        manualRefreshActiveRef.current = false;
        await new Promise((resolve) => window.setTimeout(resolve, 380));
        setManualRefreshing(false);
        setRefreshButtonText('刷新');
      }
      loadInFlightRef.current = false;
      if (queuedForceRefreshRef.current) {
        queuedForceRefreshRef.current = false;
        void loadSkills({ force: true, manual: true });
      }
    }
  };

  const loadAiSettings = async () => {
    try {
      const status = await invoke<AiSettingsStatus>('get_ai_settings_status');
      setHasApiKey(status.hasKey);
      const mask = status.maskedKey?.trim() ?? '';
      setSavedMaskedKey(mask);
      setShowMaskedKey(status.hasKey && mask.length > 0);
    } catch {
      setHasApiKey(false);
      setSavedMaskedKey('');
      setShowMaskedKey(false);
    }
  };

  const loadAppVersion = async () => {
    try {
      const version = await invoke<string>('get_app_version');
      setAppVersion(version.trim());
    } catch {
      setAppVersion('');
    }
  };

  useEffect(() => {
    void loadSkills({ initial: true });
    void loadAiSettings();
    void loadAppVersion();
    void invoke<boolean>('get_onboarding_status')
      .then((shouldShow) => {
        setShowOnboardingModal(Boolean(shouldShow));
      })
      .catch(() => {
        setShowOnboardingModal(false);
      });
  }, []);

  useEffect(() => {
    let disposed = false;
    const run = async () => {
      try {
        const update = await checkUpdater({ timeout: 12_000 });
        if (disposed || !update) {
          return;
        }
        const latest = update.version?.trim();
        if (!latest) {
          return;
        }
        try {
          if (window.localStorage.getItem(updateDismissKey(latest)) === '1') {
            return;
          }
        } catch {
          // ignore localStorage failures
        }
        const current = update.currentVersion?.trim() || appVersion || '-';
        const raw = update.rawJson as Record<string, unknown>;
        const releaseUrl =
          (typeof raw.html_url === 'string' && raw.html_url.trim()) ||
          (typeof raw.release_url === 'string' && raw.release_url.trim()) ||
          (typeof raw.releaseUrl === 'string' && raw.releaseUrl.trim()) ||
          null;
        setUpdateToast({
          currentVersion: current,
          latestVersion: latest,
          releaseUrl,
        });
      } catch {
        // ignore auto-check errors
      }
    };

    const bootTimer = window.setTimeout(() => {
      void run();
    }, 2000);
    const interval = window.setInterval(
      () => {
        void run();
      },
      6 * 60 * 60 * 1000,
    );
    return () => {
      disposed = true;
      window.clearTimeout(bootTimer);
      window.clearInterval(interval);
    };
  }, [appVersion]);

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    void listen<ScanProgressPayload>('scan_progress', (event) => {
      const payload = event.payload;
      if (!payload) {
        return;
      }

      if (!manualRefreshActiveRef.current) {
        return;
      }

      if (payload.stage === 'scanning') {
        setRefreshButtonText('扫描中...');
        return;
      }
      if (payload.stage === 'scanned') {
        setRefreshButtonText(`发现${payload.newSkillsCount}个新`);
        return;
      }
      if (payload.stage === 'summarizing') {
        const step = Math.min(payload.summarizedCount + 1, payload.summarizeTotal || 1);
        const total = Math.max(payload.summarizeTotal, 1);
        setRefreshButtonText(`总结${step}/${total}`);
        return;
      }
      if (payload.stage === 'done') {
        setRefreshButtonText('刷新完成');
        return;
      }
      if (payload.stage === 'warning' || payload.stage === 'error') {
        setRefreshButtonText('已刷新');
      }
    }).then((fn) => {
      unlisten = fn;
    });
    return () => {
      if (unlisten) {
        unlisten();
      }
    };
  }, []);

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    void listen<SkillsSnapshot>('skills_snapshot_updated', (event) => {
      const snapshot = event.payload;
      if (!snapshot) {
        return;
      }
      applySnapshot(snapshot);
    }).then((fn) => {
      unlisten = fn;
    });
    return () => {
      if (unlisten) {
        unlisten();
      }
    };
  }, []);

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    void listen<unknown>('ai_summary_stream', (event) => {
      const payload = normalizeAiSummaryStreamPayload(event.payload);
      if (!payload) {
        return;
      }

      const detail = payload.detailMarkdown ?? '';
      if (!detail) {
        return;
      }

      setPlatforms((current) =>
        current.map((platform) => {
          if (platform.id !== payload.platformId) {
            return platform;
          }
          return {
            ...platform,
            skills: platform.skills.map((skill) =>
              skill.id === payload.skillId ? { ...skill, aiDetail: detail } : skill,
            ),
          };
        }),
      );
    }).then((fn) => {
      unlisten = fn;
    });
    return () => {
      if (unlisten) {
        unlisten();
      }
    };
  }, []);

  useEffect(() => {
    if (resummarizingSkillRunningKeys.length <= 0) {
      return;
    }

    let disposed = false;
    const poll = async () => {
      try {
        const raw = await invoke<unknown>('get_ai_summary_stream_latest');
        if (disposed) {
          return;
        }
        const payload = normalizeAiSummaryStreamPayload(raw);
        if (!payload || !payload.detailMarkdown) {
          return;
        }
        setPlatforms((current) =>
          current.map((platform) => {
            if (platform.id !== payload.platformId) {
              return platform;
            }
            return {
              ...platform,
              skills: platform.skills.map((skill) =>
                skill.id === payload.skillId
                  ? { ...skill, aiDetail: payload.detailMarkdown }
                  : skill,
              ),
            };
          }),
        );
      } catch {
        // ignore poll errors
      }
    };

    void poll();
    const timer = window.setInterval(() => {
      void poll();
    }, 240);

    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [resummarizingSkillRunningKeys.length]);

  const copyText = async (key: string, text: string) => {
    if (!text) {
      return;
    }
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const textarea = document.createElement('textarea');
      textarea.value = text;
      textarea.style.position = 'fixed';
      textarea.style.opacity = '0';
      document.body.appendChild(textarea);
      textarea.select();
      document.execCommand('copy');
      document.body.removeChild(textarea);
    }
    setCopiedKey(key);
    if (copiedResetTimerRef.current) {
      window.clearTimeout(copiedResetTimerRef.current);
      copiedResetTimerRef.current = null;
    }
    copiedResetTimerRef.current = window.setTimeout(() => {
      setCopiedKey((current) => (current === key ? '' : current));
      copiedResetTimerRef.current = null;
    }, 1400);
  };

  const formatInstalledAt = (value?: number | null) => {
    if (!value) {
      return '未知';
    }
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) {
      return '未知';
    }
    return date.toLocaleString('zh-CN', { hour12: false });
  };

  const toggleFavorite = async (platformId: string, skillId: string, favorite: boolean) => {
    setError('');
    try {
      const snapshot = await invoke<SkillsSnapshot>('update_skill', {
        payload: { platformId, skillId, favorite },
      });
      applySnapshot(snapshot);
    } catch (err) {
      setError(getInvokeErrorMessage(err, '收藏更新失败'));
    }
  };

  const saveApiKey = async () => {
    setSavingKey(true);
    setSettingsMessage('');
    setError('');
    try {
      const nextApiKey = apiKeyInput.trim();
      const keepCurrentKey = showMaskedKey && hasApiKey && nextApiKey.length === 0;
      if (keepCurrentKey) {
        setSettingsMessage('Key 已保存');
        return;
      }

      const status = await invoke<AiSettingsStatus>('set_deepseek_api_key', {
        apiKey: nextApiKey,
      });
      setHasApiKey(status.hasKey);
      const mask = status.maskedKey?.trim() ?? '';
      setSavedMaskedKey(mask);
      setShowMaskedKey(status.hasKey && mask.length > 0);
      setApiKeyInput('');
      setSettingsMessage(status.hasKey ? 'Key 已保存' : 'Key 已清除');
    } catch (err) {
      const raw = getInvokeErrorMessage(err, '');
      setError(toFriendlyAiMessage(raw, '保存失败，请稍后重试。'));
    } finally {
      setSavingKey(false);
    }
  };

  const testApiKey = async () => {
    setTestingKey(true);
    setSettingsMessage('');
    setError('');
    try {
      if (apiKeyInput.trim()) {
        const status = await invoke<AiSettingsStatus>('set_deepseek_api_key', {
          apiKey: apiKeyInput,
        });
        setHasApiKey(status.hasKey);
        const mask = status.maskedKey?.trim() ?? '';
        setSavedMaskedKey(mask);
        setShowMaskedKey(status.hasKey && mask.length > 0);
        setApiKeyInput('');
      }
      await invoke<string>('test_deepseek_api_key');
      setSettingsMessage('连接测试通过，可以开始 AI 总结。');
    } catch (err) {
      const raw = getInvokeErrorMessage(err, '');
      setError(toFriendlyAiMessage(raw, '连接测试失败，请稍后重试。'));
    } finally {
      setTestingKey(false);
    }
  };

  const summarizePending = async () => {
    if (!confirmSummarizePending) {
      setConfirmSummarizePending(true);
      if (summarizePendingConfirmResetTimerRef.current) {
        window.clearTimeout(summarizePendingConfirmResetTimerRef.current);
      }
      summarizePendingConfirmResetTimerRef.current = window.setTimeout(() => {
        setConfirmSummarizePending(false);
        summarizePendingConfirmResetTimerRef.current = null;
      }, 4500);
      return;
    }
    if (summarizePendingConfirmResetTimerRef.current) {
      window.clearTimeout(summarizePendingConfirmResetTimerRef.current);
      summarizePendingConfirmResetTimerRef.current = null;
    }
    setConfirmSummarizePending(false);

    setSettingsMessage('');
    setError('');
    try {
      if (apiKeyInput.trim()) {
        const status = await invoke<AiSettingsStatus>('set_deepseek_api_key', {
          apiKey: apiKeyInput,
        });
        setHasApiKey(status.hasKey);
        const mask = status.maskedKey?.trim() ?? '';
        setSavedMaskedKey(mask);
        setShowMaskedKey(status.hasKey && mask.length > 0);
        setApiKeyInput('');
      }
      const targets = allSkills.filter((skill) => !skill.aiBrief.trim() || !skill.aiDetail.trim());
      const enqueued = enqueueResummarizeSkills(targets);
      if (enqueued.length <= 0) {
        setSummarizingAi(false);
        setSettingsMessage('没有可补全项，或相关任务已在队列中。');
        return;
      }

      summarizePendingBatchKeysRef.current = new Set(enqueued);
      setSummarizingAi(true);
      setSettingsMessage(`已加入补全队列：${enqueued.length} 个。`);
    } catch (err) {
      const raw = getInvokeErrorMessage(err, '');
      setError(toFriendlyAiMessage(raw, 'AI 总结失败，请稍后重试。'));
    }
  };

  const resummarizeAllSkills = async () => {
    if (!confirmResummarizeAll) {
      setConfirmResummarizeAll(true);
      if (resummarizeConfirmResetTimerRef.current) {
        window.clearTimeout(resummarizeConfirmResetTimerRef.current);
      }
      resummarizeConfirmResetTimerRef.current = window.setTimeout(() => {
        setConfirmResummarizeAll(false);
        resummarizeConfirmResetTimerRef.current = null;
      }, 4500);
      return;
    }
    if (resummarizeConfirmResetTimerRef.current) {
      window.clearTimeout(resummarizeConfirmResetTimerRef.current);
      resummarizeConfirmResetTimerRef.current = null;
    }
    setConfirmResummarizeAll(false);

    setSettingsMessage('');
    setError('');
    setResummarizingAllProgressText('');
    setResummarizingAllCurrentSkill('');

    const targets = allSkills.slice();
    const enqueued = enqueueResummarizeSkills(targets);
    if (enqueued.length <= 0) {
      setResummarizingAllAi(false);
      setResummarizingAllProgressText('没有可入队的任务，可能都在运行或等待中。');
      return;
    }

    resummarizeAllBatchKeysRef.current = new Set(enqueued);
    resummarizeAllBatchTotalRef.current = enqueued.length;
    resummarizeAllBatchDoneRef.current = 0;
    setResummarizingAllAi(true);
    setResummarizingAllProgressText(`全部重新总结 0/${enqueued.length}`);
    setResummarizingAllCurrentSkill('');
    setSettingsMessage(`已加入全部重总结队列：${enqueued.length} 个。`);
  };

  const checkUpdates = async () => {
    setCheckingUpdate(true);
    setUpdateMessage('');
    setError('');
    try {
      const update = await checkUpdater({ timeout: 12_000 });
      if (!update) {
        setUpdateToast(null);
        setUpdateMessage('没有可用正式更新。');
        return;
      }

      const latest = update.version?.trim() || '-';
      const current = update.currentVersion?.trim() || appVersion || '-';
      if (current && current !== '-') {
        setAppVersion(current);
      }
      const raw = update.rawJson as Record<string, unknown>;
      const releaseUrl =
        (typeof raw.html_url === 'string' && raw.html_url.trim()) ||
        (typeof raw.release_url === 'string' && raw.release_url.trim()) ||
        (typeof raw.releaseUrl === 'string' && raw.releaseUrl.trim()) ||
        null;

      setUpdateMessage(`发现新版本 ${latest}（当前 ${current}），可直接点击右下角“立即更新”。`);
      try {
        if (window.localStorage.getItem(updateDismissKey(latest)) !== '1') {
          setUpdateToast({
            currentVersion: current,
            latestVersion: latest,
            releaseUrl,
          });
        }
      } catch {
        setUpdateToast({
          currentVersion: current,
          latestVersion: latest,
          releaseUrl,
        });
      }
    } catch (err) {
      const friendly = toFriendlyUpdaterCheckMessage(err, '检查更新失败，请稍后重试。');
      if (friendly === '没有可用正式更新。') {
        setUpdateToast(null);
        setUpdateMessage('没有可用正式更新。');
        setError('');
        return;
      }
      setError(friendly);
    } finally {
      setCheckingUpdate(false);
    }
  };

  const installUpdate = async () => {
    if (!updateToast || updatingApp) {
      return;
    }

    setError('');
    setSettingsMessage('');
    setUpdatingApp(true);
    setUpdateProgressText('准备更新...');

    let downloadedBytes = 0;
    let totalBytes = 0;

    try {
      const update = await checkUpdater();
      if (!update) {
        setUpdateToast(null);
        setUpdateMessage('当前已是最新版本。');
        return;
      }

      const onEvent = (event: DownloadEvent) => {
        if (event.event === 'Started') {
          downloadedBytes = 0;
          totalBytes = event.data.contentLength ?? 0;
          setUpdateProgressText(totalBytes > 0 ? '下载中 0%' : '下载中...');
          return;
        }
        if (event.event === 'Progress') {
          downloadedBytes += event.data.chunkLength;
          if (totalBytes > 0) {
            const percent = Math.max(
              0,
              Math.min(99, Math.floor((downloadedBytes / totalBytes) * 100)),
            );
            setUpdateProgressText(`下载中 ${percent}%`);
          } else {
            setUpdateProgressText('下载中...');
          }
          return;
        }
        if (event.event === 'Finished') {
          setUpdateProgressText('安装中...');
        }
      };

      await update.downloadAndInstall(onEvent, { timeout: 120_000 });
      setUpdateProgressText('安装完成，正在重启...');
      setUpdateMessage(`新版本 ${update.version} 已安装，应用正在重启。`);
      setUpdateToast(null);
      window.setTimeout(() => {
        void invoke('restart_app');
      }, 600);
    } catch (err) {
      const raw = getInvokeErrorMessage(err, '自动更新失败，请稍后重试。');
      const friendly = toFriendlyAiMessage(raw, '自动更新失败，请稍后重试。');
      setUpdateProgressText('');
      if (updateToast.releaseUrl) {
        setError(`${friendly}，已切换到下载页面。`);
        void openUrl(updateToast.releaseUrl);
      } else {
        setError(friendly);
      }
    } finally {
      setUpdatingApp(false);
    }
  };

  const syncResummarizeSkillQueueState = () => {
    setResummarizingSkillRunningKeys(Array.from(resummarizeSkillRunningRef.current));
    setResummarizingSkillQueuedKeys(resummarizeSkillQueueRef.current.map((item) => item.key));
  };

  const syncSummaryQueueProgressState = () => {
    setSummaryQueueProgressDone(summaryQueueProgressDoneRef.current);
    setSummaryQueueProgressTotal(summaryQueueProgressTotalRef.current);
  };

  const recalcSummaryQueueProgressTotal = () => {
    const recalculatedTotal =
      summaryQueueProgressDoneRef.current +
      resummarizeSkillRunningRef.current.size +
      resummarizeSkillQueueRef.current.length;
    summaryQueueProgressTotalRef.current = Math.max(recalculatedTotal, 0);
    syncSummaryQueueProgressState();
  };

  const enqueueResummarizeSkills = (skills: FlatSkill[]): string[] => {
    const added: string[] = [];
    const queuedKeys = new Set(resummarizeSkillQueueRef.current.map((item) => item.key));
    for (const skill of skills) {
      if (resummarizeSkillRunningRef.current.has(skill.key) || queuedKeys.has(skill.key)) {
        continue;
      }
      resummarizeSkillQueueRef.current.push({
        key: skill.key,
        platformId: skill.platformId,
        skillId: skill.id,
        skillName: skill.name,
      });
      queuedKeys.add(skill.key);
      added.push(skill.key);
    }
    if (added.length > 0) {
      summaryQueueProgressTotalRef.current += added.length;
      syncSummaryQueueProgressState();
      syncResummarizeSkillQueueState();
      pumpResummarizeSkillQueue();
    }
    return added;
  };

  const onResummarizeQueueItemStarted = (item: ResummarizeQueueItem) => {
    if (resummarizeAllBatchKeysRef.current.has(item.key)) {
      const total = Math.max(resummarizeAllBatchTotalRef.current, 1);
      const done = resummarizeAllBatchDoneRef.current;
      const step = Math.min(done + 1, total);
      setResummarizingAllProgressText(`全部重新总结 ${step}/${total}`);
      setResummarizingAllCurrentSkill(item.skillName);
    }
  };

  const onResummarizeQueueItemFinished = (item: ResummarizeQueueItem) => {
    summaryQueueProgressDoneRef.current += 1;
    syncSummaryQueueProgressState();

    if (summarizePendingBatchKeysRef.current.delete(item.key)) {
      if (summarizePendingBatchKeysRef.current.size === 0) {
        setSummarizingAi(false);
        setSettingsMessage('AI 总结已完成。');
      }
    }

    if (resummarizeAllBatchKeysRef.current.delete(item.key)) {
      resummarizeAllBatchDoneRef.current += 1;
      const total = Math.max(resummarizeAllBatchTotalRef.current, 1);
      const done = Math.min(resummarizeAllBatchDoneRef.current, total);
      setResummarizingAllProgressText(`全部重新总结 ${done}/${total}`);
      if (resummarizeAllBatchKeysRef.current.size === 0) {
        setResummarizingAllAi(false);
        setResummarizingAllCurrentSkill('');
        setResummarizingAllProgressText(`全部重新总结完成 ${done}/${total}`);
        setSettingsMessage('已完成全部重新总结。');
      }
    }
  };

  const resetSummaryQueueProgress = () => {
    summaryQueueProgressDoneRef.current = 0;
    summaryQueueProgressTotalRef.current = 0;
    syncSummaryQueueProgressState();
  };

  const pumpResummarizeSkillQueue = () => {
    if (resummarizeQueuePumpActiveRef.current) {
      return;
    }
    resummarizeQueuePumpActiveRef.current = true;

    while (
      resummarizeSkillRunningRef.current.size < resummarizeSkillConcurrencyLimitRef.current &&
      resummarizeSkillQueueRef.current.length > 0
    ) {
      const item = resummarizeSkillQueueRef.current.shift();
      if (!item) {
        continue;
      }
      if (resummarizeSkillRunningRef.current.has(item.key)) {
        continue;
      }

      resummarizeSkillRunningRef.current.add(item.key);
      syncResummarizeSkillQueueState();
      onResummarizeQueueItemStarted(item);

      void (async () => {
        try {
          const snapshot = await invoke<SkillsSnapshot>('resummarize_skill', {
            payload: {
              platformId: item.platformId,
              skillId: item.skillId,
            },
          });
          applySnapshot(snapshot);
          resummarizeSkillConcurrencyLimitRef.current = Math.min(
            RESUMMARIZE_SKILL_MAX_CONCURRENT,
            resummarizeSkillConcurrencyLimitRef.current + 1,
          );
        } catch (err) {
          const raw = getInvokeErrorMessage(err, '');
          const friendly = toFriendlyAiMessage(raw, '重新总结失败，请稍后重试。');
          const lower = raw.toLowerCase();
          if (raw.includes('AI 总结任务已停止')) {
            return;
          }
          if (
            raw.includes('429') ||
            lower.includes('rate limit') ||
            lower.includes('timeout') ||
            lower.includes('network') ||
            lower.includes('connection') ||
            lower.includes('http 5')
          ) {
            resummarizeSkillConcurrencyLimitRef.current = Math.max(
              RESUMMARIZE_SKILL_MIN_CONCURRENT,
              resummarizeSkillConcurrencyLimitRef.current - 1,
            );
          }
          setError(friendly);
        } finally {
          onResummarizeQueueItemFinished(item);
          resummarizeSkillRunningRef.current.delete(item.key);
          syncResummarizeSkillQueueState();
          if (
            resummarizeSkillRunningRef.current.size <= 0 &&
            resummarizeSkillQueueRef.current.length <= 0
          ) {
            resetSummaryQueueProgress();
          }
          resummarizeQueuePumpActiveRef.current = false;
          pumpResummarizeSkillQueue();
        }
      })();
    }

    resummarizeQueuePumpActiveRef.current = false;
  };

  const resummarizeSkill = (skill: FlatSkill) => {
    setError('');
    setShowSkillActionMenu(false);
    void enqueueResummarizeSkills([skill]);
  };

  const openSkillContextMenu = (event: MouseEvent<HTMLDivElement>, skill: FlatSkill) => {
    event.preventDefault();
    event.stopPropagation();
    setShowSkillActionMenu(false);

    const menuWidth = 180;
    const menuHeight = 132;
    const x = Math.max(8, Math.min(event.clientX, window.innerWidth - menuWidth - 8));
    const y = Math.max(8, Math.min(event.clientY, window.innerHeight - menuHeight - 8));
    setSkillContextMenu({
      skillKey: skill.key,
      x,
      y,
    });
  };

  const stopAllSummaries = async () => {
    if (!hasSummaryQueueActivity || stoppingAllSummaries) {
      return;
    }

    setStoppingAllSummaries(true);
    setError('');
    setSettingsMessage('');

    summarizePendingBatchKeysRef.current.clear();
    resummarizeAllBatchKeysRef.current.clear();
    resummarizeAllBatchTotalRef.current = 0;
    resummarizeAllBatchDoneRef.current = 0;
    setSummarizingAi(false);
    setResummarizingAllAi(false);
    setResummarizingAllCurrentSkill('');
    setResummarizingAllProgressText('');

    resummarizeSkillQueueRef.current = [];
    recalcSummaryQueueProgressTotal();
    syncResummarizeSkillQueueState();
    if (resummarizeSkillRunningRef.current.size <= 0) {
      resetSummaryQueueProgress();
    }

    try {
      const affected = await invoke<number>('cancel_ai_summary_jobs');
      setSettingsMessage(`已停止总结任务。运行中中断 ${affected} 个，队列已清空。`);
    } catch (err) {
      setError(getInvokeErrorMessage(err, '停止任务失败，请稍后重试。'));
    } finally {
      setStoppingAllSummaries(false);
    }
  };

  const dismissOnboarding = async () => {
    setShowOnboardingModal(false);
    try {
      await invoke('complete_onboarding');
    } catch {
      // ignore onboarding persistence failures
    }
  };

  const dismissUpdateToast = () => {
    if (updateToast?.latestVersion) {
      try {
        window.localStorage.setItem(updateDismissKey(updateToast.latestVersion), '1');
      } catch {
        // ignore localStorage failures
      }
    }
    setUpdateToast(null);
  };

  return (
    <div className="ui-panel relative flex h-full w-full flex-col overflow-hidden">
      <div className="border-b border-[var(--line)] bg-[var(--panel-strong)] px-3 py-2">
        <div className="flex items-center gap-2">
          <div className="flex h-8 flex-1 items-center rounded-md border border-[var(--line-strong)] bg-white px-2.5 md:max-w-[420px]">
            <Search className="mr-2 h-4 w-4 text-zinc-400" />
            <input
              type="text"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              className="h-full w-full border-none bg-transparent text-sm outline-none placeholder:text-zinc-400"
              placeholder="搜索 skill"
            />
          </div>
          <button
            onClick={() => void loadSkills({ force: true, manual: true })}
            className="inline-flex h-8 items-center gap-1 rounded-md border border-[var(--line-strong)] bg-white px-2.5 text-xs text-zinc-700 hover:bg-zinc-50 disabled:opacity-50"
            disabled={loading || manualRefreshing}
          >
            <RefreshCcw
              className={['h-3.5 w-3.5', manualRefreshing || refreshing ? 'animate-spin' : ''].join(' ')}
            />
            {manualRefreshing ? refreshButtonText : '刷新'}
          </button>
          <div className="ml-auto flex items-center gap-2">
            <div className="hidden items-center gap-3 text-[11px] text-zinc-600 md:flex">
              <div>Skills {allSkills.length}</div>
              <div>已总结 {aiSummarizedCount}</div>
              <div className={aiPendingCount > 0 ? 'font-semibold text-rose-600' : ''}>
                待总结 {aiPendingCount}
              </div>
            </div>
            {hasSummaryQueueActivity && (
              <>
                <div className="inline-flex items-center gap-1.5 text-[11px] text-zinc-600">
                  <span>总结进度</span>
                  <span className="rounded bg-sky-100 px-1.5 py-0.5 font-semibold text-sky-700">
                    {Math.min(summaryQueueProgressDone, Math.max(summaryQueueProgressTotal, 1))}/
                    {Math.max(summaryQueueProgressTotal, 1)}
                  </span>
                  <span>队列中</span>
                  <span
                    className={[
                      'rounded px-1.5 py-0.5 font-semibold',
                      resummarizingSkillQueuedKeys.length > 0
                        ? 'bg-amber-100 text-amber-700'
                        : 'bg-zinc-100 text-zinc-600',
                    ].join(' ')}
                  >
                    {resummarizingSkillQueuedKeys.length}
                  </span>
                </div>
                <button
                  type="button"
                  onClick={() => void stopAllSummaries()}
                  className="inline-flex h-8 items-center rounded-md border border-rose-300 bg-rose-50 px-2.5 text-xs font-medium text-rose-700 hover:bg-rose-100 disabled:opacity-60"
                  disabled={stoppingAllSummaries}
                  title="停止当前所有总结任务并清空队列"
                >
                  {stoppingAllSummaries ? '停止中...' : '停止全部'}
                </button>
              </>
            )}
            <button
              type="button"
              onClick={() => setShowSettings((v) => !v)}
              className="mr-1 inline-flex h-8 w-8 items-center justify-center rounded-md border border-[var(--line-strong)] bg-white text-zinc-600 transition-colors hover:bg-zinc-50 hover:text-zinc-900"
              title="设置"
            >
              <Settings className="h-3.5 w-3.5" />
            </button>
          </div>
        </div>
        {showSettings && (
          <div className="mt-2 rounded-md border border-[var(--line)] bg-white px-4 py-3 text-sm text-zinc-800">
            <div className="grid grid-cols-[110px_1fr] gap-y-2">
              <div className="flex min-h-8 items-center text-xs leading-6 text-zinc-500">DeepSeek Key</div>
              <div className="space-y-1">
                <div className="flex items-center gap-2">
                  <input
                    type={showMaskedKey ? 'text' : 'password'}
                    value={showMaskedKey ? savedMaskedKey : apiKeyInput}
                    onFocus={() => {
                      if (showMaskedKey) {
                        window.setTimeout(() => {
                          const active = document.activeElement;
                          if (active instanceof HTMLInputElement) {
                            active.select();
                          }
                        }, 0);
                      }
                    }}
                    onBlur={() => {
                      if (!apiKeyInput.trim() && hasApiKey && savedMaskedKey) {
                        setShowMaskedKey(true);
                      }
                    }}
                    onChange={(e) => {
                      setShowMaskedKey(false);
                      setApiKeyInput(e.target.value);
                    }}
                    className="h-8 w-[360px] max-w-full rounded border border-[var(--line-strong)] px-2 text-[13px] text-zinc-800 outline-none focus:border-zinc-700"
                    placeholder={hasApiKey ? '已配置，可直接测试或输入新 Key 覆盖' : '请输入 sk-...'}
                  />
                  <button
                    type="button"
                    onClick={() => void testApiKey()}
                    className="inline-flex h-8 items-center rounded border border-[var(--line-strong)] bg-white px-3 text-[12px] text-zinc-700 hover:bg-zinc-50 disabled:opacity-50"
                    disabled={testingKey}
                  >
                    {testingKey ? '测试中...' : '测试连通'}
                  </button>
                  <button
                    type="button"
                    onClick={() => void saveApiKey()}
                    className="inline-flex h-8 items-center rounded border border-[var(--line-strong)] bg-white px-3 text-[12px] text-zinc-700 hover:bg-zinc-50 disabled:opacity-50"
                    disabled={savingKey}
                  >
                    {savingKey ? '保存中...' : '保存 Key'}
                  </button>
                  <div className="text-[12px] leading-6 text-zinc-500">
                    当前状态：{hasApiKey ? '已配置 Key' : '未配置 Key'}
                  </div>
                  <button
                    type="button"
                    className="h-8 text-[12px] font-medium text-sky-700 underline underline-offset-2 hover:text-sky-800"
                    onClick={() => {
                      void openUrl('https://platform.deepseek.com/');
                    }}
                  >
                    访问 DeepSeek 开放平台
                  </button>
                </div>
              </div>
              <div className="flex min-h-8 items-center text-xs leading-6 text-zinc-500">AI总结</div>
              <div className="flex min-h-8 flex-wrap items-center gap-4">
                <div className="flex items-center gap-2">
                  <div className="text-[12px] leading-6 text-zinc-500">已总结</div>
                  <div className="text-[13px] leading-6 text-zinc-800">{aiSummarizedCount}</div>
                  <button
                    type="button"
                    onClick={() => void resummarizeAllSkills()}
                    className={[
                      'inline-flex h-8 items-center rounded border px-3 text-[12px] disabled:opacity-50',
                      confirmResummarizeAll
                        ? 'border-rose-300 bg-rose-50 font-semibold text-rose-700 hover:bg-rose-100'
                        : 'border-[var(--line-strong)] bg-white text-zinc-700 hover:bg-zinc-50',
                    ].join(' ')}
                    title="重新总结全部 skill（覆盖现有 AI 总结）"
                  >
                    {resummarizingAllAi
                      ? '全部总结中...'
                      : confirmResummarizeAll
                        ? '再次点击确认开始，会覆盖全部 skills 已有总结'
                        : '全部重新总结'}
                  </button>
                </div>
                <div className="flex items-center gap-2">
                  <div className="text-[12px] leading-6 text-zinc-500">未总结</div>
                  <div
                    className={[
                      'text-[13px] leading-6',
                      aiPendingCount > 0 ? 'font-semibold text-rose-600' : 'text-zinc-800',
                    ].join(' ')}
                  >
                    {aiPendingCount}
                  </div>
                  <button
                    type="button"
                    onClick={() => void summarizePending()}
                    className={[
                      'inline-flex h-8 items-center rounded border px-3 text-[12px] disabled:opacity-50',
                      confirmSummarizePending
                        ? 'border-rose-300 bg-rose-50 font-semibold text-rose-700 hover:bg-rose-100'
                        : 'border-[var(--line-strong)] bg-white text-zinc-700 hover:bg-zinc-50',
                    ].join(' ')}
                    title="补全未总结 skill"
                  >
                    {summarizingAi
                      ? '总结中...'
                      : confirmSummarizePending
                        ? '再次点击确认开始，仅补全未总结'
                        : '补全未总结'}
                  </button>
                </div>
                {resummarizingAllProgressText && (
                  <div className="text-[12px] leading-6 text-zinc-500">
                    {resummarizingAllProgressText}
                    {resummarizingAllCurrentSkill ? ` · ${resummarizingAllCurrentSkill}` : ''}
                  </div>
                )}
              </div>
              <div className="flex min-h-8 items-center text-xs leading-6 text-zinc-500">当前版本</div>
              <div className="flex min-h-8 flex-wrap items-center gap-2">
                <div className="text-[13px] leading-6 text-zinc-800">{appVersion || '-'}</div>
                <button
                  type="button"
                  onClick={() => void checkUpdates()}
                  className="inline-flex h-8 items-center rounded border border-[var(--line-strong)] bg-white px-3 text-[12px] text-zinc-700 hover:bg-zinc-50 disabled:opacity-50"
                  disabled={checkingUpdate}
                >
                  {checkingUpdate ? '检查中...' : '检查更新'}
                </button>
              </div>
            </div>
            {settingsMessage && (
              <div className="mt-2 rounded border border-emerald-200 bg-emerald-50 px-2 py-1 text-[12px] font-medium leading-6 text-emerald-700">
                {settingsMessage}
              </div>
            )}
            {updateMessage && (
              <div className="mt-2 rounded border border-sky-200 bg-sky-50 px-2 py-1 text-[12px] leading-6 text-sky-700">
                {updateMessage}
              </div>
            )}
          </div>
        )}
        {error && <div className="mt-2 text-xs text-red-600">{error}</div>}
      </div>

      <div className="grid min-h-0 flex-1 md:grid-cols-[340px_minmax(0,1fr)]">
        <div className="min-h-0 border-r border-[var(--line)] bg-white">
          <div className="custom-scrollbar overlay-scroll h-full overflow-auto">
            {loading ? (
              <div className="px-3 py-2 text-sm text-zinc-500">加载中...</div>
            ) : filteredSkills.length === 0 ? (
              <div className="px-3 py-2 text-sm text-zinc-500">没有匹配内容</div>
            ) : (
              <div>
                {filteredSkills.map((skill) => {
                  const isSelected = selectedSkillKey === skill.key;
                  const isSkillRunning = summarizingRunningSkillKeys.has(skill.key);
                  const isSkillQueued = !isSkillRunning && summarizingQueuedSkillKeys.has(skill.key);
                  return (
                    <div
                      key={skill.key}
                      onClick={() => setSelectedSkillKey(skill.key)}
                      onContextMenu={(event) => openSkillContextMenu(event, skill)}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter' || e.key === ' ') {
                          e.preventDefault();
                          setSelectedSkillKey(skill.key);
                        }
                      }}
                      role="button"
                      tabIndex={0}
                      className={[
                        'w-full select-none border-b border-[var(--line)] px-3 py-2.5 text-left transition-colors outline-none focus:bg-zinc-50',
                        isSelected
                          ? 'relative z-[1] bg-zinc-100 before:pointer-events-none before:absolute before:inset-y-0 before:left-0 before:w-[2px] before:bg-zinc-900 before:content-[\'\']'
                          : 'bg-white hover:bg-zinc-50',
                      ].join(' ')}
                    >
                      <div className="flex min-w-0 flex-col gap-0.5">
                        <div className="flex items-center justify-between gap-2">
                          <div className="flex min-w-0 items-center gap-1.5">
                            {isSkillRunning && (
                              <RefreshCcw
                                className="h-3.5 w-3.5 shrink-0 animate-spin text-zinc-500"
                              />
                            )}
                            {isSkillQueued && (
                              <span className="shrink-0 rounded border border-amber-200 bg-amber-50 px-1.5 py-0 text-[11px] font-medium leading-4 text-amber-700">
                                排队中
                              </span>
                            )}
                            <div className="truncate text-[15px] font-semibold leading-5 text-zinc-900">
                              {skill.name}
                            </div>
                          </div>
                          <button
                            type="button"
                            className="rounded p-0.5 text-zinc-400 transition-colors hover:bg-zinc-100 hover:text-zinc-800"
                            onClick={(e) => {
                              e.stopPropagation();
                              void toggleFavorite(skill.platformId, skill.id, !skill.favorite);
                            }}
                            title={skill.favorite ? '取消收藏' : '收藏'}
                          >
                            <Star
                              className={[
                                'h-4 w-4',
                                skill.favorite ? 'fill-current text-zinc-800' : 'text-zinc-400',
                              ].join(' ')}
                            />
                          </button>
                        </div>
                        <div className="truncate text-xs leading-4 text-zinc-500">
                          {skill.id}
                        </div>
                        <div className="truncate pt-0.5 text-[13px] leading-5 text-zinc-700">
                          {skill.aiBrief || ''}
                        </div>
                      </div>
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        </div>

        <div className="min-h-0 bg-[var(--panel)]">
          <div className="custom-scrollbar overlay-scroll h-full overflow-auto">
            {!selectedSkill ? (
              <div className="px-4 py-3 text-sm text-zinc-500">请选择一个 skill</div>
            ) : (
              <>
                <div className="border-b border-[var(--line)] px-4 py-2">
                  <div className="flex items-center justify-between gap-3">
                    <div>
                      <div className="text-lg font-semibold text-zinc-900">{selectedSkill.name}</div>
                      <div className="text-xs text-zinc-500">{selectedSkill.id}</div>
                    </div>
                    <div className="relative ml-auto" ref={skillActionMenuRef}>
                      <button
                        type="button"
                        onClick={() => setShowSkillActionMenu((current) => !current)}
                        className="inline-flex h-8 w-8 items-center justify-center rounded-md border border-[var(--line-strong)] bg-white text-zinc-600 transition-colors hover:bg-zinc-50 hover:text-zinc-900"
                        title="更多操作"
                        aria-label="更多操作"
                        aria-haspopup="menu"
                        aria-expanded={showSkillActionMenu}
                      >
                        <MoreHorizontal className="h-3.5 w-3.5" />
                      </button>
                      {showSkillActionMenu && (
                        <div className="absolute right-0 top-9 z-30 min-w-[118px] rounded-md border border-[var(--line)] bg-white p-1 shadow-lg">
                          <button
                            type="button"
                            onClick={() => void resummarizeSkill(selectedSkill)}
                            className="inline-flex w-full items-center gap-1.5 rounded px-2 py-1.5 text-left text-[12px] text-zinc-700 hover:bg-zinc-100 disabled:opacity-50"
                            disabled={isSelectedSkillSummarizing}
                            title="使用 AI 重新总结当前 skill"
                          >
                            {isSelectedSkillRunning && (
                              <RefreshCcw className="h-3.5 w-3.5 animate-spin" />
                            )}
                            {isSelectedSkillQueued && (
                              <span className="rounded border border-amber-200 bg-amber-50 px-1 py-0 text-[10px] font-medium leading-4 text-amber-700">
                                排队中
                              </span>
                            )}
                            {isSelectedSkillRunning
                              ? '总结中...'
                              : isSelectedSkillQueued
                                ? '排队中...'
                                : '重新总结'}
                          </button>
                        </div>
                      )}
                    </div>
                  </div>
                </div>
                <div className="space-y-1.5 px-4 py-3">
                  <div className="grid grid-cols-[90px_1fr] gap-y-0 text-xs">
                    <div className="leading-6 text-zinc-500">安装时间</div>
                    <div className="mono-ui leading-6 text-zinc-700">
                      {formatInstalledAt(selectedSkill.installedAt)}
                    </div>

                    <div className="leading-6 text-zinc-500">技能路径</div>
                    <div className="grid grid-cols-[minmax(0,1fr)_auto] items-start gap-1.5">
                      <div className="mono-ui break-all leading-6 text-zinc-700">
                        {selectedSkill.path}
                      </div>
                      <button
                        type="button"
                        className="mt-1 rounded border border-[var(--line)] bg-white p-0.5 text-zinc-500 transition-colors hover:bg-zinc-50 hover:text-zinc-800"
                        onClick={() => void copyText('skillPath', selectedSkill.path)}
                        title="复制路径"
                      >
                        {copiedKey === 'skillPath' ? (
                          <Check className="h-3 w-3" />
                        ) : (
                          <Copy className="h-3 w-3" />
                        )}
                      </button>
                    </div>

                  </div>

                  <div className="border-t border-[var(--line)] pt-3">
                    <div className="mb-2 text-xs text-zinc-500">一句话说明</div>
                    <div
                      className={[
                        'whitespace-pre-wrap rounded-md px-3 py-2 leading-6',
                        hasSelectedAiBrief
                          ? 'border border-zinc-300 bg-zinc-50 text-[15px] font-semibold text-zinc-900'
                          : 'border border-dashed border-amber-300 bg-amber-50 text-[13px] font-medium text-amber-700',
                      ].join(' ')}
                    >
                      {hasSelectedAiBrief ? selectedSkill.aiBrief : emptyAiHint}
                    </div>
                  </div>

                  <div className="border-t border-[var(--line)] pt-3">
                    <div className="mb-2 inline-flex items-center rounded-md border border-[var(--line)] bg-zinc-100 p-0.5">
                      <button
                        type="button"
                        className={[
                          'no-hover-btn inline-flex items-center gap-1 rounded-[6px] px-2.5 py-1 text-xs',
                          contentTab === 'detail'
                            ? 'border border-zinc-300 bg-white text-zinc-900'
                            : 'border border-transparent text-zinc-600',
                        ].join(' ')}
                        onClick={() => setContentTab('detail')}
                      >
                        <span className="inline-flex items-center gap-1">
                          {isSelectedSkillRunning && <RefreshCcw className="h-3 w-3 animate-spin" />}
                          {isSelectedSkillQueued && (
                            <span className="rounded border border-amber-200 bg-amber-50 px-1 py-0 text-[10px] font-medium leading-4 text-amber-700">
                              排队中
                            </span>
                          )}
                          AI总结
                        </span>
                      </button>
                      <button
                        type="button"
                        className={[
                          'no-hover-btn inline-flex items-center rounded-[6px] px-2.5 py-1 text-xs',
                          contentTab === 'source'
                            ? 'border border-zinc-300 bg-white text-zinc-900'
                            : 'border border-transparent text-zinc-600',
                        ].join(' ')}
                        onClick={() => setContentTab('source')}
                      >
                        原始全文（SKILL.md）
                      </button>
                    </div>
                    {contentTab === 'detail' ? (
                      hasSelectedAiDetail ? (
                        <div className="markdown-skill markdown-pane rounded-md border border-[var(--line)] bg-white text-sm leading-7 text-zinc-700">
                          <ReactMarkdown remarkPlugins={[remarkGfm]} components={markdownComponents}>
                            {selectedSkill.aiDetail}
                          </ReactMarkdown>
                        </div>
                      ) : (
                        <div className="rounded-md border border-dashed border-amber-300 bg-amber-50 px-3 py-2 text-sm leading-6 text-amber-700">
                          {emptyAiHint}
                        </div>
                      )
                    ) : selectedSkill.sourceMarkdown.trim() ? (
                      <div className="markdown-skill markdown-pane rounded-md border border-[var(--line)] bg-white text-sm leading-7 text-zinc-700">
                        <ReactMarkdown remarkPlugins={[remarkGfm]} components={markdownComponents}>
                          {selectedSkill.sourceMarkdown}
                        </ReactMarkdown>
                      </div>
                    ) : (
                      <div className="whitespace-pre-wrap text-sm leading-6 text-zinc-700">
                        {selectedSkill.sourceDescription ||
                          selectedSkill.sourceUsage ||
                          '该 skill 没有原始描述内容'}
                      </div>
                    )}
                  </div>

                  {!selectedSkill.sourceDescription &&
                    selectedSkill.sourceUsage &&
                    selectedSkill.sourceCommands.length === 0 && (
                      <div className="border-t border-[var(--line)] pt-3">
                        <div className="mb-2 text-xs text-zinc-500">使用提示</div>
                        <div className="whitespace-pre-wrap text-sm leading-6 text-zinc-800">
                          {selectedSkill.sourceUsage}
                        </div>
                      </div>
                    )}
                  
                  <div className="text-[11px] leading-5 text-zinc-400">
                    默认展示“AI总结”，可切换查看 SKILL.md 原始全文。
                  </div>
                </div>
              </>
            )}
          </div>
        </div>
      </div>

      {skillContextMenu && contextMenuSkill && (
        <div
          className="fixed z-50 min-w-[172px] rounded-md border border-[var(--line)] bg-white p-1 shadow-xl"
          style={{ left: `${skillContextMenu.x}px`, top: `${skillContextMenu.y}px` }}
          onMouseDown={(event) => event.stopPropagation()}
        >
          <button
            type="button"
            className="block w-full rounded px-2 py-1.5 text-left text-[12px] text-zinc-700 transition-colors hover:bg-zinc-100 hover:text-zinc-900"
            onClick={() => {
              void copyText('context-skill-path', contextMenuSkill.path);
              setSkillContextMenu(null);
            }}
          >
            复制技能路径
          </button>
          <button
            type="button"
            className="block w-full rounded px-2 py-1.5 text-left text-[12px] text-zinc-700 transition-colors hover:bg-zinc-100 hover:text-zinc-900"
            onClick={() => {
              void copyText('context-skill-name', contextMenuSkill.name);
              setSkillContextMenu(null);
            }}
          >
            复制技能名字
          </button>
          <div className="my-1 border-t border-[var(--line)]" />
          <button
            type="button"
            className="block w-full rounded px-2 py-1.5 text-left text-[12px] text-zinc-700 transition-colors hover:bg-zinc-100 hover:text-zinc-900 disabled:opacity-50"
            disabled={isContextMenuSkillSummarizing}
            onClick={() => {
              setSkillContextMenu(null);
              resummarizeSkill(contextMenuSkill);
            }}
          >
            {isContextMenuSkillSummarizing ? '总结中...' : '重新总结'}
          </button>
        </div>
      )}

      {updateToast && (
        <div className="pointer-events-none absolute bottom-4 right-4 z-40">
          <div className="pointer-events-auto w-[360px] rounded-lg border border-sky-200 bg-white p-3 shadow-2xl">
            <div className="text-sm font-semibold text-sky-700">
              发现新版本 {updateToast.latestVersion}
            </div>
            <div className="mt-1 text-xs leading-5 text-zinc-600">
              当前版本 {updateToast.currentVersion}，建议更新到最新版本。
            </div>
            <div className="mt-3 flex justify-end gap-2">
              <button
                type="button"
                className="inline-flex h-7 items-center rounded border border-sky-300 bg-sky-50 px-2.5 text-[12px] text-sky-700 hover:bg-sky-100 disabled:opacity-60"
                onClick={() => {
                  void installUpdate();
                }}
                disabled={updatingApp}
              >
                {updatingApp ? updateProgressText || '更新中...' : '立即更新'}
              </button>
              <button
                type="button"
                className="inline-flex h-7 items-center rounded border border-[var(--line-strong)] bg-white px-2.5 text-[12px] text-zinc-700 hover:bg-zinc-50"
                onClick={dismissUpdateToast}
                disabled={updatingApp}
              >
                稍后
              </button>
            </div>
          </div>
        </div>
      )}

      {showOnboardingModal && (
        <div className="absolute inset-0 z-50 flex items-center justify-center bg-black/35 p-4">
          <div className="w-full max-w-[680px] rounded-lg border border-[var(--line)] bg-white p-5 shadow-2xl">
            <div className="text-[18px] font-semibold text-zinc-900">欢迎使用 SkillsBox</div>
            <div className="mt-2 text-sm leading-7 text-zinc-700">
              <div>首次使用建议按下面步骤：</div>
              <div>1. 先在设置中配置并测试 DeepSeek API Key。</div>
              <div>2. 点击“补全未总结”，生成 AI 一句话说明和 AI 总结内容。</div>
              <div>3. 给常用 skill 点收藏，之后可在菜单栏直接点击复制技能路径，粘贴给任意 AI 调用。</div>
              <div>4. 新安装的 skills 会自动检测并加入列表；配置了 API 后会自动总结。</div>
              <div>5. 右侧可随时切换查看“AI总结”和“原始全文（SKILL.md）”。</div>
            </div>
            <div className="mt-4 flex justify-end">
              <button
                type="button"
                onClick={() => void dismissOnboarding()}
                className="inline-flex h-8 items-center rounded border border-[var(--line-strong)] bg-white px-3 text-[13px] text-zinc-700 hover:bg-zinc-50"
              >
                我知道了
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
