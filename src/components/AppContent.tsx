import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { useEffect, useMemo, useRef, useState } from 'react';
import { Check, Copy, RefreshCcw, Search, Settings, Star } from 'lucide-react';

interface Skill {
  id: string;
  name: string;
  sourceName: string;
  sourceUsage: string;
  sourceDescription: string;
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

interface FlatSkill extends Skill {
  key: string;
  platformId: string;
  platformName: string;
  platformRoot: string;
  searchText: string;
}

interface ScanProgressPayload {
  stage: string;
  message: string;
  newSkillsCount: number;
  summarizedCount: number;
  summarizeTotal: number;
  currentSkill?: string | null;
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

function toFriendlyAiMessage(message: string, fallback: string): string {
  const text = message.trim();
  const lower = text.toLowerCase();

  if (!text) {
    return fallback;
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
  const [scannedAt, setScannedAt] = useState<number | null>(null);
  const [aiSummarizedCount, setAiSummarizedCount] = useState(0);
  const [aiPendingCount, setAiPendingCount] = useState(0);
  const [apiKeyInput, setApiKeyInput] = useState('');
  const [hasApiKey, setHasApiKey] = useState(false);
  const [savedMaskedKey, setSavedMaskedKey] = useState('');
  const [showMaskedKey, setShowMaskedKey] = useState(false);
  const [savingKey, setSavingKey] = useState(false);
  const [testingKey, setTestingKey] = useState(false);
  const [summarizingAi, setSummarizingAi] = useState(false);
  const [resummarizingSkillKey, setResummarizingSkillKey] = useState('');
  const [settingsMessage, setSettingsMessage] = useState('');
  const loadInFlightRef = useRef(false);
  const queuedForceRefreshRef = useRef(false);
  const manualRefreshActiveRef = useRef(false);
  const manualRefreshStartedAtRef = useRef(0);
  const [debouncedSearchQuery, setDebouncedSearchQuery] = useState('');

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
            `${skill.name} ${skill.id} ${skill.aiBrief} ${skill.aiDetail} ${skill.sourceUsage} ${skill.sourceDescription} ${skill.path} ${platform.name}`.toLowerCase(),
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
  }, [selectedSkillKey]);

  const applySnapshot = (snapshot: SkillsSnapshot) => {
    setPlatforms(snapshot.platforms);
    setScannedAt(snapshot.scannedAt ?? null);
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

  useEffect(() => {
    void loadSkills({ initial: true });
    void loadAiSettings();
  }, []);

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    void listen<ScanProgressPayload>('scan_progress', (event) => {
      const payload = event.payload;
      if (!payload || !manualRefreshActiveRef.current) {
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
    window.setTimeout(() => {
      setCopiedKey((current) => (current === key ? '' : current));
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
      const status = await invoke<AiSettingsStatus>('set_deepseek_api_key', {
        apiKey: apiKeyInput,
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
    setSummarizingAi(true);
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
      const snapshot = await invoke<SkillsSnapshot>('summarize_pending_skills');
      applySnapshot(snapshot);
      setSettingsMessage('AI 总结已完成。');
    } catch (err) {
      const raw = getInvokeErrorMessage(err, '');
      setError(toFriendlyAiMessage(raw, 'AI 总结失败，请稍后重试。'));
    } finally {
      setSummarizingAi(false);
    }
  };

  const resummarizeSkill = async (skill: FlatSkill) => {
    setError('');
    setResummarizingSkillKey(skill.key);
    try {
      const snapshot = await invoke<SkillsSnapshot>('resummarize_skill', {
        payload: {
          platformId: skill.platformId,
          skillId: skill.id,
        },
      });
      applySnapshot(snapshot);
    } catch (err) {
      const raw = getInvokeErrorMessage(err, '');
      setError(toFriendlyAiMessage(raw, '重新总结失败，请稍后重试。'));
    } finally {
      setResummarizingSkillKey('');
    }
  };

  return (
    <div className="ui-panel flex h-full w-full flex-col overflow-hidden">
      <div className="drag-region border-b border-[var(--line)] bg-[var(--panel-strong)] px-3 py-2">
        <div className="no-drag flex items-center gap-2">
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
              <div>待总结 {aiPendingCount}</div>
            </div>
            <button
              type="button"
              onClick={() => setShowSettings((v) => !v)}
              className="inline-flex h-8 w-8 items-center justify-center rounded-md border border-[var(--line-strong)] bg-white text-zinc-600 hover:text-zinc-900"
              title="设置"
            >
              <Settings className="h-3.5 w-3.5" />
            </button>
          </div>
        </div>
        {showSettings && (
          <div className="no-drag mt-2 border border-[var(--line)] bg-white px-4 py-3 text-sm text-zinc-800">
            <div className="grid grid-cols-[110px_1fr] gap-y-2">
              <div className="text-[13px] leading-6 text-zinc-500">DeepSeek Key</div>
              <div className="space-y-1">
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
                  className="w-full rounded border border-[var(--line-strong)] px-2 py-1.5 text-[13px] text-zinc-800 outline-none focus:border-zinc-700"
                  placeholder={hasApiKey ? '已配置，可直接测试或输入新 Key 覆盖' : '请输入 sk-...'}
                />
                <div className="flex flex-wrap items-center gap-2">
                  <button
                    type="button"
                    onClick={() => void testApiKey()}
                    className="inline-flex h-7 items-center rounded border border-[var(--line-strong)] bg-white px-3 text-[12px] text-zinc-700 hover:bg-zinc-50 disabled:opacity-50"
                    disabled={testingKey}
                  >
                    {testingKey ? '测试中...' : '测试连通'}
                  </button>
                  <button
                    type="button"
                    onClick={() => void saveApiKey()}
                    className="inline-flex h-7 items-center rounded border border-[var(--line-strong)] bg-white px-3 text-[12px] text-zinc-700 hover:bg-zinc-50 disabled:opacity-50"
                    disabled={savingKey}
                  >
                    {savingKey ? '保存中...' : '保存 Key'}
                  </button>
                </div>
                <div className="text-[12px] leading-6 text-zinc-500">
                  当前状态：{hasApiKey ? '已配置 Key' : '未配置 Key'}
                </div>
              </div>
              <div className="text-[13px] leading-6 text-zinc-500">Skills 总结</div>
              <div>
                <button
                  type="button"
                  onClick={() => void summarizePending()}
                  className="inline-flex h-7 items-center rounded border border-[var(--line-strong)] bg-white px-3 text-[12px] text-zinc-700 hover:bg-zinc-50 disabled:opacity-50"
                  disabled={summarizingAi}
                >
                  {summarizingAi ? '总结中...' : 'AI 总结'}
                </button>
              </div>
              <div className="text-[13px] leading-6 text-zinc-500">最近扫描</div>
              <div className="text-[13px] leading-6 text-zinc-800">{formatInstalledAt(scannedAt)}</div>
              <div className="text-[13px] leading-6 text-zinc-500">已 AI 总结</div>
              <div className="text-[13px] leading-6 text-zinc-800">{aiSummarizedCount}</div>
              <div className="text-[13px] leading-6 text-zinc-500">未 AI 总结</div>
              <div className="text-[13px] leading-6 text-zinc-800">{aiPendingCount}</div>
            </div>
            {settingsMessage && (
              <div className="mt-2 rounded border border-emerald-200 bg-emerald-50 px-2 py-1 text-[12px] font-medium leading-6 text-emerald-700">
                {settingsMessage}
              </div>
            )}
          </div>
        )}
        {error && <div className="no-drag mt-2 text-xs text-red-600">{error}</div>}
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
                  return (
                    <div
                      key={skill.key}
                      onClick={() => setSelectedSkillKey(skill.key)}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter' || e.key === ' ') {
                          e.preventDefault();
                          setSelectedSkillKey(skill.key);
                        }
                      }}
                      role="button"
                      tabIndex={0}
                      className={[
                        'w-full cursor-pointer border-b border-[var(--line)] px-3 py-2.5 text-left transition-colors outline-none focus:bg-zinc-50',
                        isSelected ? 'bg-zinc-100' : 'bg-white hover:bg-zinc-50',
                      ].join(' ')}
                    >
                      <div className="flex min-w-0 flex-col gap-0.5">
                        <div className="flex items-center justify-between gap-2">
                          <div className="truncate text-[15px] font-semibold leading-5 text-zinc-900">
                            {skill.name}
                          </div>
                          <button
                            type="button"
                            className="no-drag p-0 text-zinc-400 hover:text-zinc-800"
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
                        <div className="truncate text-xs leading-4 text-zinc-500">{skill.id}</div>
                        <div className="truncate pt-0.5 text-[13px] leading-5 text-zinc-700">
                          {skill.aiBrief ||
                            skill.sourceDescription ||
                            skill.sourceUsage ||
                            '正在生成简介...'}
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
          <div className="custom-scrollbar h-full overflow-y-auto">
            {!selectedSkill ? (
              <div className="px-4 py-3 text-sm text-zinc-500">请选择一个 skill</div>
            ) : (
              <>
                <div className="border-b border-[var(--line)] px-4 py-2">
                  <div className="flex items-start justify-between gap-3">
                    <div>
                      <div className="text-lg font-semibold text-zinc-900">{selectedSkill.name}</div>
                      <div className="text-xs text-zinc-500">{selectedSkill.id}</div>
                    </div>
                    <button
                      type="button"
                      onClick={() => void resummarizeSkill(selectedSkill)}
                      className="inline-flex h-7 items-center gap-1 rounded border border-[var(--line-strong)] bg-white px-2.5 text-[12px] text-zinc-700 hover:bg-zinc-50 disabled:opacity-50"
                      disabled={resummarizingSkillKey === selectedSkill.key}
                      title="使用 AI 重新总结当前 skill"
                    >
                      <RefreshCcw
                        className={[
                          'h-3.5 w-3.5',
                          resummarizingSkillKey === selectedSkill.key ? 'animate-spin' : '',
                        ].join(' ')}
                      />
                      {resummarizingSkillKey === selectedSkill.key ? '总结中...' : '重新总结'}
                    </button>
                  </div>
                </div>
                <div className="space-y-3 px-4 py-3">
                  <div className="grid grid-cols-[90px_1fr] gap-y-0 text-xs">
                    <div className="leading-6 text-zinc-500">所属分类</div>
                    <div className="leading-6 text-zinc-800">{selectedSkill.platformName}</div>
                    <div className="leading-6 text-zinc-500">安装时间</div>
                    <div className="leading-6 text-zinc-800">
                      {formatInstalledAt(selectedSkill.installedAt)}
                    </div>

                    <div className="leading-6 text-zinc-500">分类目录</div>
                    <div className="grid grid-cols-[minmax(0,1fr)_auto] items-start gap-1.5">
                      <div className="mono-ui break-all leading-6 text-zinc-700">
                        {selectedSkill.platformRoot}
                      </div>
                      <button
                        type="button"
                        className="mt-1 rounded border border-[var(--line)] bg-white p-0.5 text-zinc-500 hover:text-zinc-800"
                        onClick={() => void copyText('platformRoot', selectedSkill.platformRoot)}
                        title="复制目录"
                      >
                        {copiedKey === 'platformRoot' ? (
                          <Check className="h-3 w-3" />
                        ) : (
                          <Copy className="h-3 w-3" />
                        )}
                      </button>
                    </div>

                    <div className="leading-6 text-zinc-500">技能路径</div>
                    <div className="grid grid-cols-[minmax(0,1fr)_auto] items-start gap-1.5">
                      <div className="mono-ui break-all leading-6 text-zinc-700">
                        {selectedSkill.path}
                      </div>
                      <button
                        type="button"
                        className="mt-1 rounded border border-[var(--line)] bg-white p-0.5 text-zinc-500 hover:text-zinc-800"
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

                    <div className="leading-6 text-zinc-500">定义文件</div>
                    <div className="grid grid-cols-[minmax(0,1fr)_auto] items-start gap-1.5">
                      <div className="mono-ui break-all leading-6 text-zinc-700">
                        {selectedSkill.definitionPath}
                      </div>
                      <button
                        type="button"
                        className="mt-1 rounded border border-[var(--line)] bg-white p-0.5 text-zinc-500 hover:text-zinc-800"
                        onClick={() => void copyText('definitionPath', selectedSkill.definitionPath)}
                        title="复制定义文件路径"
                      >
                        {copiedKey === 'definitionPath' ? (
                          <Check className="h-3 w-3" />
                        ) : (
                          <Copy className="h-3 w-3" />
                        )}
                      </button>
                    </div>
                  </div>

                  <div className="border-t border-[var(--line)] pt-3">
                    <div className="mb-2 text-xs text-zinc-500">一句话说明</div>
                    <div className="whitespace-pre-wrap rounded-md border border-zinc-300 bg-zinc-50 px-3 py-2 text-[15px] font-semibold leading-6 text-zinc-900">
                      {selectedSkill.aiBrief ||
                        selectedSkill.sourceDescription ||
                        selectedSkill.sourceUsage ||
                        '该 skill 暂无说明'}
                    </div>
                  </div>

                  <div className="border-t border-[var(--line)] pt-3">
                    <div className="mb-2 text-xs text-zinc-500">详细说明</div>
                    <div className="whitespace-pre-wrap text-sm leading-6 text-zinc-800">
                      {selectedSkill.aiDetail ||
                        selectedSkill.sourceDescription ||
                        selectedSkill.sourceUsage ||
                        '该 skill 没有描述内容'}
                    </div>
                  </div>

                  {selectedSkill.sourceCommands.length > 0 && (
                    <div className="border-t border-[var(--line)] pt-3">
                      <div className="mb-2 text-xs text-zinc-500">常用命令</div>
                      <div className="space-y-1">
                        {selectedSkill.sourceCommands.map((command, index) => (
                          <div
                            key={`${command}-${index}`}
                            className="grid grid-cols-[minmax(0,1fr)_auto] items-start gap-1.5 rounded-md border border-[var(--line)] bg-white px-2 py-1"
                          >
                            <div className="mono-ui break-all text-[11px] leading-5 text-zinc-700">
                              {command}
                            </div>
                            <button
                              type="button"
                              className="mt-0.5 shrink-0 rounded border border-[var(--line)] bg-white p-0.5 text-zinc-500 hover:text-zinc-800"
                              onClick={() => void copyText(`cmd-${index}`, command)}
                              title="复制命令"
                            >
                              {copiedKey === `cmd-${index}` ? (
                                <Check className="h-3 w-3" />
                              ) : (
                                <Copy className="h-3 w-3" />
                              )}
                            </button>
                          </div>
                        ))}
                      </div>
                    </div>
                  )}

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

                  <div className="border-t border-[var(--line)] pt-3">
                    <div className="mb-2 text-xs text-zinc-500">原始描述（SKILL.md）</div>
                    <div className="whitespace-pre-wrap text-sm leading-6 text-zinc-700">
                      {selectedSkill.sourceDescription ||
                        selectedSkill.sourceUsage ||
                        '该 skill 没有原始描述内容'}
                    </div>
                  </div>
                </div>
              </>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
