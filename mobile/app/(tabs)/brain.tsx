import { LinearGradient } from 'expo-linear-gradient';
import { useRouter } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import React, { useEffect, useMemo, useState } from 'react';
import { Pressable, ScrollView, TextInput, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import Animated, { FadeIn, FadeOut, SlideInDown } from 'react-native-reanimated';

import type { EntityType } from '@/api';
import { showingSampleData } from '@/api/client';
import { BrainMap, DemoNotice, EntityNode, ProfileButton, Txt } from '@/components';
import { alpha } from '@/lib/color';
import { haptics } from '@/lib/haptics';
import { useResponsive } from '@/lib/useResponsive';
import { useAppStore } from '@/store/appStore';
import { entityColor, gradients, palette, radius, sky, spacing } from '@/theme/tokens';

/**
 * Entities — full-bleed night sky. The map IS the screen; search, filters,
 * selection card and the pinned strip float above it. Hold a planet and
 * drag to rearrange your world; positions are remembered.
 *
 * The sky is dark in both themes on purpose (like a photo viewer), so every
 * colour here comes from the `sky` tokens rather than the light/dark theme —
 * theme text on it would vanish in light mode.
 */

const FILTERS: { key: EntityType | 'all'; label: string }[] = [
  { key: 'all', label: 'All' },
  { key: 'project', label: 'Projects' },
  { key: 'company', label: 'Companies' },
  { key: 'person', label: 'People' },
  { key: 'mission', label: 'Missions' },
  { key: 'agent', label: 'Agents' },
  { key: 'file', label: 'Files' },
  { key: 'memory', label: 'Memories' },
];

const OVERLAY_BG = sky.overlay;
const OVERLAY_BORDER = sky.overlayBorder;

export default function EntitiesScreen() {
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const { isWide } = useResponsive();

  const entities = useAppStore((s) => s.entities);
  const loadEntities = useAppStore((s) => s.loadEntities);
  const togglePinned = useAppStore((s) => s.togglePinned);
  const nodeOffsets = useAppStore((s) => s.nodeOffsets);
  const setNodeOffset = useAppStore((s) => s.setNodeOffset);

  const [query, setQuery] = useState('');
  const [filter, setFilter] = useState<EntityType | 'all'>('all');
  const [selectedId, setSelectedId] = useState<string | null>(null);
  // The sample world, in demo mode — say so, like every other sample screen.
  const [sample, setSample] = useState(false);

  useEffect(() => {
    if (entities.length === 0) loadEntities();
  }, [entities.length, loadEntities]);

  useEffect(() => {
    void showingSampleData().then(setSample);
  }, [entities]);

  const filtered = useMemo(() => {
    return entities.filter((e) => {
      const matchesType = filter === 'all' || e.type === filter;
      const matchesQuery =
        query.trim() === '' || e.name.toLowerCase().includes(query.toLowerCase());
      return matchesType && matchesQuery;
    });
  }, [entities, filter, query]);

  const selected = useMemo(
    () => (selectedId ? entities.find((e) => e.id === selectedId) ?? null : null),
    [entities, selectedId],
  );

  const pinned = useMemo(() => entities.filter((e) => e.pinned), [entities]);
  // "Recent" only when there are real timestamps to sort by. Live entities
  // carry none, so they keep the gateway's own order under an honest label
  // instead of a recency sort on invented times.
  const dated = entities.some((e) => e.lastActiveAt);
  const recent = useMemo(
    () =>
      dated
        ? [...entities]
            .sort((a, b) => Date.parse(b.lastActiveAt ?? '') - Date.parse(a.lastActiveAt ?? ''))
            .slice(0, 8)
        : entities.slice(0, 8),
    [entities, dated],
  );
  const stripLabel = pinned.length > 0 ? 'Pinned' : dated ? 'Recent' : 'Your entities';

  const open = (id: string) => {
    setSelectedId(null);
    router.push(`/entity/${id}`);
  };

  const headerTop = insets.top + spacing[2];
  // Clear of title + search + filter row (+ the sample-data notice).
  const controlsTop = headerTop + 176 + (sample && entities.length > 0 ? 48 : 0);

  return (
    <View style={{ flex: 1, backgroundColor: sky.bg }}>
      {/* The map is always dark — force light status-bar icons here. */}
      <StatusBar style="light" />

      {/* Full-bleed sky */}
      <View style={{ position: 'absolute', top: 0, left: 0, right: 0, bottom: 0 }}>
        <BrainMap
          entities={filtered}
          selectedId={selectedId}
          onSelectNode={setSelectedId}
          nodeOffsets={nodeOffsets}
          onNodeMove={setNodeOffset}
          controlsTopOffset={controlsTop}
        />
      </View>

      {/* Floating header: title · search · filters */}
      <View pointerEvents="box-none" style={{ position: 'absolute', top: headerTop, left: 0, right: 0, gap: spacing[2], paddingHorizontal: spacing[4] }}>
        <View style={{ flexDirection: 'row', alignItems: 'flex-end', justifyContent: 'space-between' }}>
          <View>
            <Txt variant="title" style={{ color: sky.text }}>
              Entities
            </Txt>
            <Txt variant="caption" style={{ color: sky.textSecondary, marginTop: 2 }}>
              {filtered.length} of {entities.length} entities · hold a planet to move it
            </Txt>
          </View>
          <View style={{ flexDirection: 'row', gap: spacing[2] }}>
            <Pressable
              onPress={() => {
                haptics.press();
                router.push('/canvas');
              }}
              accessibilityRole="button"
              accessibilityLabel="Open your canvas"
              style={{
                paddingHorizontal: spacing[3],
                height: 36,
                borderRadius: radius.full,
                alignItems: 'center',
                justifyContent: 'center',
                backgroundColor: OVERLAY_BG,
                borderWidth: 1,
                borderColor: OVERLAY_BORDER,
              }}
            >
              <Txt variant="label" style={{ color: sky.textSecondary }}>▦ Canvas</Txt>
            </Pressable>
            <Pressable
              onPress={() => {
                haptics.press();
                router.push('/companion');
              }}
              accessibilityRole="button"
              accessibilityLabel="Talk by voice"
              style={{
                paddingHorizontal: spacing[3],
                height: 36,
                borderRadius: radius.full,
                alignItems: 'center',
                justifyContent: 'center',
                backgroundColor: alpha(palette.copper, 0.24),
                borderWidth: 1,
                borderColor: alpha(palette.copper, 0.6),
              }}
            >
              <Txt variant="label" style={{ color: sky.accentText }}>◉ Voice</Txt>
            </Pressable>
            <ProfileButton size={36} />
          </View>
        </View>

        <View
          style={{
            flexDirection: 'row',
            alignItems: 'center',
            gap: spacing[2],
            backgroundColor: OVERLAY_BG,
            borderRadius: radius.lg,
            borderWidth: 1,
            borderColor: OVERLAY_BORDER,
            paddingHorizontal: spacing[3],
            height: 42,
          }}
        >
          <Txt style={{ color: sky.textSecondary, fontSize: 15 }}>⌕</Txt>
          <TextInput
            value={query}
            onChangeText={setQuery}
            placeholder="Search your entities…"
            placeholderTextColor={sky.textMuted}
            style={{ flex: 1, color: sky.text, fontSize: 15 }}
          />
          {query ? (
            <Pressable onPress={() => setQuery('')} hitSlop={8} accessibilityLabel="Clear search">
              <Txt style={{ color: sky.textSecondary, fontSize: 13 }}>✕</Txt>
            </Pressable>
          ) : null}
        </View>

        <ScrollView horizontal showsHorizontalScrollIndicator={false} contentContainerStyle={{ gap: spacing[2] }}>
          {FILTERS.map((f) => {
            const active = filter === f.key;
            return (
              <Pressable
                key={f.key}
                onPress={() => {
                  haptics.tap();
                  setFilter(f.key);
                  setSelectedId(null);
                }}
                style={{
                  paddingHorizontal: spacing[3],
                  paddingVertical: spacing[2],
                  borderRadius: radius.full,
                  backgroundColor: active ? alpha(palette.copper, 0.24) : OVERLAY_BG,
                  borderWidth: 1,
                  borderColor: active ? alpha(palette.copper, 0.6) : OVERLAY_BORDER,
                }}
              >
                <Txt variant="label" style={{ color: active ? sky.accentText : sky.textSecondary }}>
                  {f.label}
                </Txt>
              </Pressable>
            );
          })}
        </ScrollView>

        {sample && entities.length > 0 ? <DemoNotice /> : null}
      </View>

      {/* Selection card */}
      {selected ? (
        <Animated.View
          entering={SlideInDown.duration(240)}
          exiting={FadeOut.duration(140)}
          style={{
            position: 'absolute',
            left: spacing[4],
            right: spacing[4],
            bottom: (isWide ? spacing[4] : insets.bottom + 74) + 92,
          }}
        >
          <View
            style={{
              borderRadius: radius.xl,
              backgroundColor: sky.overlayStrong,
              borderWidth: 1,
              borderColor: OVERLAY_BORDER,
              padding: spacing[4],
              gap: spacing[3],
            }}
          >
            <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[3] }}>
              <EntityNode type={selected.type} color={entityColor(selected)} size={44} />
              <View style={{ flex: 1 }}>
                <Txt variant="heading" style={{ color: sky.text, fontSize: 17 }} numberOfLines={1}>
                  {selected.name}
                </Txt>
                <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2], marginTop: 2 }}>
                  <View
                    style={{
                      paddingHorizontal: 8,
                      paddingVertical: 2,
                      borderRadius: radius.full,
                      backgroundColor: alpha(entityColor(selected), 0.18),
                    }}
                  >
                    <Txt variant="caption" style={{ color: entityColor(selected), fontSize: 11 }} weight="semibold">
                      {selected.personal ? 'Personal' : selected.type[0].toUpperCase() + selected.type.slice(1)}
                    </Txt>
                  </View>
                  {[selected.stage, selected.fileCount ? `${selected.fileCount} file${selected.fileCount === 1 ? '' : 's'}` : undefined]
                    .filter(Boolean)
                    .map((bit) => (
                      <Txt key={bit} variant="caption" style={{ color: sky.textSecondary }}>
                        {bit}
                      </Txt>
                    ))}
                </View>
              </View>
              <Pressable
                onPress={() => setSelectedId(null)}
                hitSlop={8}
                accessibilityLabel="Close"
                style={{ width: 30, height: 30, borderRadius: 15, alignItems: 'center', justifyContent: 'center', backgroundColor: sky.control }}
              >
                <Txt style={{ color: sky.textSecondary, fontSize: 13 }}>✕</Txt>
              </Pressable>
            </View>

            {selected.summary ? (
              <Txt variant="body" style={{ color: sky.textSecondary, lineHeight: 20 }} numberOfLines={2}>
                {selected.summary}
              </Txt>
            ) : null}

            <View style={{ flexDirection: 'row', gap: spacing[2] }}>
              <Pressable
                onPress={() => {
                  haptics.press();
                  open(selected.id);
                }}
                style={{ flex: 1 }}
                accessibilityRole="button"
                accessibilityLabel="Open workspace"
              >
                <LinearGradient
                  colors={gradients.primaryButton}
                  start={{ x: 0, y: 0 }}
                  end={{ x: 0, y: 1 }}
                  style={{ height: 42, borderRadius: radius.md, alignItems: 'center', justifyContent: 'center', flexDirection: 'row', gap: 6 }}
                >
                  <Txt style={{ color: palette.white, fontSize: 14 }} weight="semibold">
                    Open workspace
                  </Txt>
                  <Txt style={{ color: palette.white, fontSize: 14 }}>→</Txt>
                </LinearGradient>
              </Pressable>
              <Pressable
                onPress={() => {
                  haptics.press();
                  setSelectedId(null);
                  router.push(`/canvas?entityId=${encodeURIComponent(selected.id)}`);
                }}
                accessibilityRole="button"
                accessibilityLabel="Open canvas"
                style={{
                  width: 42,
                  height: 42,
                  borderRadius: radius.md,
                  alignItems: 'center',
                  justifyContent: 'center',
                  backgroundColor: sky.control,
                  borderWidth: 1,
                  borderColor: OVERLAY_BORDER,
                }}
              >
                <Txt style={{ fontSize: 15, color: sky.textSecondary }}>▦</Txt>
              </Pressable>
              <Pressable
                onPress={() => {
                  haptics.success();
                  togglePinned(selected.id);
                }}
                accessibilityRole="button"
                accessibilityLabel={selected.pinned ? 'Unpin' : 'Pin'}
                style={{
                  width: 42,
                  height: 42,
                  borderRadius: radius.md,
                  alignItems: 'center',
                  justifyContent: 'center',
                  backgroundColor: sky.control,
                  borderWidth: 1,
                  borderColor: OVERLAY_BORDER,
                }}
              >
                <Txt style={{ fontSize: 16, color: selected.pinned ? palette.champagne : sky.textSecondary }}>
                  {selected.pinned ? '★' : '☆'}
                </Txt>
              </Pressable>
            </View>
          </View>
        </Animated.View>
      ) : null}

      {/* Pinned / recent strip — floats above the tab bar */}
      <Animated.View
        entering={FadeIn}
        pointerEvents="box-none"
        style={{
          position: 'absolute',
          left: 0,
          right: 0,
          bottom: isWide ? spacing[4] : insets.bottom + 74,
        }}
      >
        <Txt
          variant="caption"
          weight="bold"
          style={{ paddingHorizontal: spacing[5], letterSpacing: 1, textTransform: 'uppercase', marginBottom: spacing[2], color: sky.textMuted }}
        >
          {stripLabel}
        </Txt>
        <ScrollView
          horizontal
          showsHorizontalScrollIndicator={false}
          contentContainerStyle={{ gap: spacing[3], paddingHorizontal: spacing[5] }}
        >
          {(pinned.length > 0 ? pinned : recent).map((e) => (
            <Pressable
              key={e.id}
              onPress={() => {
                haptics.press();
                open(e.id);
              }}
              style={{
                alignItems: 'center',
                width: 72,
                paddingVertical: spacing[2],
                borderRadius: radius.lg,
                backgroundColor: OVERLAY_BG,
                borderWidth: 1,
                borderColor: OVERLAY_BORDER,
              }}
            >
              <EntityNode type={e.type} color={entityColor(e)} size={42} />
              <Txt variant="caption" numberOfLines={1} style={{ color: sky.textSecondary, maxWidth: 64, marginTop: 2, fontSize: 10 }}>
                {e.name}
              </Txt>
            </Pressable>
          ))}
        </ScrollView>
      </Animated.View>
    </View>
  );
}
