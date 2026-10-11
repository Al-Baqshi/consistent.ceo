/**
 * Skills: the playbooks your agents load on demand.
 *
 * The phone browses, reads and switches skills on or off; writing and
 * importing them happens on the computer, where the dashboard has an editor
 * and the file system. A switch here takes effect on the very next chat turn
 * or agent run — the gateway reads skills fresh every time.
 */
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { ActivityIndicator, Pressable, Switch, View } from 'react-native';

import {
  getSkill,
  listSkills,
  setSkillEnabled,
  skillsErrorMessage,
  type SkillDetail,
  type SkillSummary,
  type SkillsList,
} from '@/api/skills';
import { DemoNotice, Screen, SectionHeader, Txt } from '@/components';
import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

type Group = { key: string; title: string; caption: string; skills: SkillSummary[] };

export default function SkillsScreen() {
  const theme = useTheme();
  const [data, setData] = useState<SkillsList | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [pending, setPending] = useState<Record<string, boolean>>({});
  const [rowError, setRowError] = useState<string | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);
  const [detail, setDetail] = useState<SkillDetail | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);

  const load = useCallback(async () => {
    try {
      setLoadError(null);
      setData(await listSkills());
    } catch (e) {
      setLoadError(skillsErrorMessage(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const onRefresh = async () => {
    setRefreshing(true);
    await load();
    setRefreshing(false);
  };

  const groups: Group[] = useMemo(() => {
    if (!data) return [];
    const name = (id?: string) => data.entities.find((e) => e.id === id)?.name ?? 'Project';
    const out: Group[] = [];
    const workspace = data.skills.filter((s) => s.scope === 'global');
    if (workspace.length) {
      out.push({ key: 'global', title: 'Your workspace', caption: 'Offered in every venture', skills: workspace });
    }
    const byEntity = new Map<string, SkillSummary[]>();
    for (const s of data.skills) {
      if (s.scope !== 'entity' || !s.entityId) continue;
      byEntity.set(s.entityId, [...(byEntity.get(s.entityId) ?? []), s]);
    }
    [...byEntity.entries()]
      .sort((a, b) => name(a[0]).localeCompare(name(b[0])))
      .forEach(([entityId, skills]) =>
        out.push({ key: entityId, title: name(entityId), caption: 'Only while this project is active', skills }),
      );
    const builtin = data.skills.filter((s) => s.scope === 'builtin');
    if (builtin.length) {
      out.push({
        key: 'builtin',
        title: 'The Consistent CEO playbook',
        caption: 'Built in — turn any of them off',
        skills: builtin,
      });
    }
    return out;
  }, [data]);

  const activeCount = data?.skills.filter((s) => s.active).length ?? 0;

  const toggle = async (skill: SkillSummary, next: boolean) => {
    if (!data || data.demo || pending[skill.id]) return;
    haptics.tap();
    setRowError(null);
    setPending((p) => ({ ...p, [skill.id]: true }));
    setData({
      ...data,
      skills: data.skills.map((s) => (s.id === skill.id ? { ...s, enabled: next, active: next && s.active } : s)),
    });
    try {
      await setSkillEnabled(skill.id, next);
      // Shadowing between scopes can change with one switch — re-read it all.
      await load();
    } catch (e) {
      setData((d) => (d ? { ...d, skills: d.skills.map((s) => (s.id === skill.id ? skill : s)) } : d));
      setRowError(skillsErrorMessage(e));
      haptics.warning();
    } finally {
      setPending((p) => {
        const { [skill.id]: _done, ...rest } = p;
        return rest;
      });
    }
  };

  const open = async (skill: SkillSummary) => {
    haptics.tap();
    if (openId === skill.id) {
      setOpenId(null);
      setDetail(null);
      return;
    }
    setOpenId(skill.id);
    setDetail(null);
    setDetailLoading(true);
    try {
      setDetail(await getSkill(skill.id));
    } catch {
      setDetail(null);
    } finally {
      setDetailLoading(false);
    }
  };

  return (
    <Screen
      scroll
      onRefresh={onRefresh}
      refreshing={refreshing}
      title="Skills"
      subtitle={data ? `${activeCount} active · playbooks your agents load on demand` : 'Playbooks your agents load on demand'}
    >
      {data?.demo ? <DemoNotice style={{ marginBottom: spacing[3] }} /> : null}

      {!data && !loadError ? (
        <View style={{ paddingVertical: spacing[8], alignItems: 'center' }}>
          <ActivityIndicator color={theme.colors.accent} />
        </View>
      ) : null}

      {loadError ? (
        <Txt variant="body" tone="danger" style={{ marginBottom: spacing[3] }}>
          {loadError}
        </Txt>
      ) : null}
      {rowError ? (
        <Txt variant="caption" tone="danger" style={{ marginBottom: spacing[3] }}>
          {rowError}
        </Txt>
      ) : null}

      {groups.map((group) => (
        <View key={group.key} style={{ marginBottom: spacing[5] }}>
          <SectionHeader title={group.title} caption={group.caption} />
          <View style={{ gap: spacing[2] }}>
            {group.skills.map((skill) => {
              const expanded = openId === skill.id;
              return (
                <View
                  key={skill.id}
                  style={{
                    borderRadius: radius.lg,
                    borderWidth: 1,
                    borderColor: expanded ? theme.colors.accent + '55' : theme.colors.border,
                    backgroundColor: theme.colors.bgCard,
                    opacity: skill.active ? 1 : 0.6,
                  }}
                >
                  <View style={{ flexDirection: 'row', alignItems: 'flex-start', gap: spacing[3], padding: spacing[4] }}>
                    <Pressable
                      onPress={() => void open(skill)}
                      accessibilityRole="button"
                      accessibilityLabel={`${expanded ? 'Collapse' : 'Read'} ${skill.title}`}
                      style={{ flex: 1, gap: spacing[1] }}
                    >
                      <Txt variant="label" weight="semibold">
                        {skill.title}
                      </Txt>
                      <Txt variant="caption" tone="secondary" numberOfLines={expanded ? undefined : 3} style={{ lineHeight: 18 }}>
                        {skill.description || 'No description — the agent will never pick this skill.'}
                      </Txt>
                      {skill.shadowedBy ? (
                        <Txt variant="caption" tone="muted">
                          Replaced by your own copy
                        </Txt>
                      ) : null}
                    </Pressable>
                    <Switch
                      value={skill.enabled}
                      onValueChange={(v) => void toggle(skill, v)}
                      disabled={Boolean(data?.demo) || Boolean(pending[skill.id])}
                      trackColor={{ true: theme.colors.accent, false: theme.colors.border }}
                      accessibilityLabel={`${skill.enabled ? 'Turn off' : 'Turn on'} ${skill.title}`}
                    />
                  </View>

                  {expanded ? (
                    <View
                      style={{
                        borderTopWidth: 1,
                        borderTopColor: theme.colors.border,
                        padding: spacing[4],
                        gap: spacing[2],
                      }}
                    >
                      {detailLoading ? (
                        <ActivityIndicator color={theme.colors.accent} />
                      ) : detail ? (
                        <Instructions body={detail.body} />
                      ) : (
                        <Txt variant="caption" tone="muted">
                          {data?.demo
                            ? 'Pair with your computer to read the full instructions.'
                            : 'The instructions couldn’t be loaded — pull to refresh.'}
                        </Txt>
                      )}
                    </View>
                  ) : null}
                </View>
              );
            })}
          </View>
        </View>
      ))}

      {data ? (
        <Txt variant="caption" tone="muted" center style={{ marginTop: spacing[2], lineHeight: 18 }}>
          Write, import and edit skills on your computer — or just say in chat:{'\n'}“save how we did this as a skill.”
        </Txt>
      ) : null}
    </Screen>
  );
}

/** Skill instructions, lightly formatted: headings, bullets, code, prose. */
function Instructions({ body }: { body: string }) {
  const blocks: React.ReactNode[] = [];
  let fence: string[] | null = null;
  body.split('\n').forEach((line, i) => {
    if (/^\s*```/.test(line)) {
      if (fence) {
        blocks.push(
          <Txt key={`code-${i}`} variant="mono" tone="secondary" style={{ fontSize: 12, lineHeight: 18 }}>
            {fence.join('\n')}
          </Txt>,
        );
        fence = null;
      } else {
        fence = [];
      }
      return;
    }
    if (fence) {
      fence.push(line);
      return;
    }
    const trimmed = line.trim();
    if (!trimmed) return;
    const heading = /^#{1,4}\s+(.*)$/.exec(trimmed);
    if (heading) {
      blocks.push(
        <Txt key={i} variant="label" weight="semibold" style={{ marginTop: spacing[2] }}>
          {heading[1].replace(/\*\*/g, '')}
        </Txt>,
      );
      return;
    }
    const item = /^(?:[-*]|\d+[.)])\s+(.*)$/.exec(trimmed);
    const text = (item ? item[1] : trimmed).replace(/\*\*|`/g, '');
    blocks.push(
      <Txt key={i} variant="caption" tone="secondary" style={{ lineHeight: 19, paddingLeft: item ? spacing[3] : 0 }}>
        {item ? `•  ${text}` : text}
      </Txt>,
    );
  });
  return <View style={{ gap: spacing[1] }}>{blocks}</View>;
}
