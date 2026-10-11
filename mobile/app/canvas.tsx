import { useLocalSearchParams, useRouter } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { ActivityIndicator, Pressable, TextInput, View, useWindowDimensions } from 'react-native';
import { Gesture, GestureDetector } from 'react-native-gesture-handler';
import Animated, {
  FadeIn,
  FadeOut,
  SlideInDown,
  runOnJS,
  useAnimatedStyle,
  useSharedValue,
} from 'react-native-reanimated';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import type { CanvasData, CanvasNode } from '@/api/canvas';
import {
  createCanvasEdge,
  createCanvasNode,
  deleteCanvasEdge,
  deleteCanvasNode,
  getCanvas,
  updateCanvasNode,
} from '@/api/canvas';
import { NeedsComputer, Txt } from '@/components';
import { EntityPickerSheet } from '@/components/companion/EntityPickerSheet';
import { alpha } from '@/lib/color';
import { haptics } from '@/lib/haptics';
import { useDemoMode } from '@/lib/useDemoMode';
import { useAppStore } from '@/store/appStore';
import { palette, radius, sky, spacing } from '@/theme/tokens';

/**
 * Live canvas — the Excalidraw-class surface, except nodes are REAL:
 * notes persist to the gateway, and task/mission/entity nodes mirror the
 * live record's current status. Defaults to the viewer's private Personal
 * Space; pass ?entityId= to open a business entity's canvas.
 *
 * World model: node (x, y) live in world coordinates. The content container
 * is a WORLD×WORLD plane whose origin sits at its centre, transformed as a
 * whole for pan/zoom, so Skia edges and nodes move together at 60fps.
 */

const WORLD = 4000;
const ORIGIN = WORLD / 2;
const MIN_SCALE = 0.4;
const MAX_SCALE = 2.5;

const BG = sky.bg;
const CARD_BG = sky.overlayStrong;
const OVERLAY_BG = sky.overlay;
const OVERLAY_BORDER = sky.overlayBorder;
const ACCENT = palette.copper;
const EDGE_COLOR = alpha(palette.copper, 0.45);

const KIND_ACCENT: Record<string, string> = {
  note: palette.champagne,
  text: sky.textSecondary,
  link: palette.slate,
  shape: sky.textSecondary,
  task: palette.moss,
  mission: palette.copper,
  entity: palette.smoke,
};

const NODE_W = 176;
const NODE_H = 78;

function nodeSize(n: CanvasNode): { w: number; h: number } {
  return { w: n.w ?? NODE_W, h: n.h ?? NODE_H };
}

interface NodeViewProps {
  node: CanvasNode;
  selected: boolean;
  linkArmed: boolean;
  scale: { value: number };
  onSelect: (id: string) => void;
  onMoved: (id: string, x: number, y: number) => void;
}

function NodeView({ node, selected, linkArmed, scale, onSelect, onMoved }: NodeViewProps) {
  const posX = useSharedValue(node.x);
  const posY = useSharedValue(node.y);

  useEffect(() => {
    posX.value = node.x;
    posY.value = node.y;
  }, [node.x, node.y, posX, posY]);

  const select = useCallback(() => onSelect(node.id), [node.id, onSelect]);
  const moved = useCallback(
    (x: number, y: number) => onMoved(node.id, x, y),
    [node.id, onMoved],
  );

  const drag = useMemo(
    () =>
      Gesture.Pan()
        .minDistance(6)
        .onChange((e) => {
          posX.value += e.changeX / scale.value;
          posY.value += e.changeY / scale.value;
        })
        .onEnd(() => {
          runOnJS(moved)(posX.value, posY.value);
        }),
    [moved, posX, posY, scale],
  );
  const tap = useMemo(
    () =>
      Gesture.Tap().onEnd((_e, success) => {
        if (success) runOnJS(select)();
      }),
    [select],
  );
  const gesture = useMemo(() => Gesture.Exclusive(drag, tap), [drag, tap]);

  const { w, h } = nodeSize(node);
  const style = useAnimatedStyle(() => ({
    position: 'absolute',
    left: ORIGIN + posX.value - w / 2,
    top: ORIGIN + posY.value - h / 2,
    width: w,
    minHeight: h,
  }));

  const kindColor = node.color ?? KIND_ACCENT[node.kind] ?? sky.textSecondary;
  const title =
    node.live && node.live.exists
      ? node.live.title
      : node.label ?? node.content ?? node.kind;
  const dead = node.live ? !node.live.exists : false;

  return (
    <GestureDetector gesture={gesture}>
      <Animated.View
        style={[
          style,
          {
            borderRadius: radius.lg,
            backgroundColor: CARD_BG,
            borderWidth: selected || linkArmed ? 1.5 : 1,
            borderColor: linkArmed ? ACCENT : selected ? alpha(kindColor, 0.9) : OVERLAY_BORDER,
            padding: spacing[3],
            gap: 4,
            opacity: dead ? 0.55 : 1,
          },
        ]}
      >
        <View style={{ flexDirection: 'row', alignItems: 'center', gap: 6 }}>
          <View style={{ width: 8, height: 8, borderRadius: 4, backgroundColor: kindColor }} />
          <Txt variant="caption" style={{ color: sky.textMuted, fontSize: 10, letterSpacing: 1 }}>
            {node.kind.toUpperCase()}
          </Txt>
          {node.live ? (
            <View
              style={{
                marginLeft: 'auto',
                paddingHorizontal: 6,
                paddingVertical: 1,
                borderRadius: radius.full,
                backgroundColor: alpha(kindColor, 0.16),
              }}
            >
              <Txt variant="caption" style={{ color: kindColor, fontSize: 9 }} weight="semibold">
                {dead ? 'deleted' : node.live.status}
              </Txt>
            </View>
          ) : null}
        </View>
        <Txt style={{ color: sky.text, fontSize: 13, lineHeight: 17 }} numberOfLines={3}>
          {title || 'Untitled'}
        </Txt>
      </Animated.View>
    </GestureDetector>
  );
}

export default function CanvasScreen() {
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const { width: screenW, height: screenH } = useWindowDimensions();
  const params = useLocalSearchParams<{ entityId?: string }>();
  const entityParam = typeof params.entityId === 'string' ? params.entityId : undefined;

  const [data, setData] = useState<CanvasData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [linkFrom, setLinkFrom] = useState<string | null>(null);
  const [labelDraft, setLabelDraft] = useState('');
  const [entityPickerOpen, setEntityPickerOpen] = useState(false);
  const appEntities = useAppStore((st) => st.entities);
  const loadEntities = useAppStore((st) => st.loadEntities);
  // Canvases are stored on the computer; demo mode explains that instead of
  // showing a failed fetch.
  const demo = useDemoMode();

  useEffect(() => {
    if (appEntities.length === 0) loadEntities();
  }, [appEntities.length, loadEntities]);

  // Viewport transform. Start centred on the world origin.
  const tx = useSharedValue(screenW / 2 - ORIGIN);
  const ty = useSharedValue(screenH / 2 - ORIGIN);
  const scale = useSharedValue(1);
  const savedTx = useSharedValue(0);
  const savedTy = useSharedValue(0);
  const savedScale = useSharedValue(1);

  const load = useCallback(async () => {
    try {
      setError(null);
      const canvas = await getCanvas(entityParam);
      setData(canvas);
    } catch {
      // The raw status line ("GET /api/canvas → 500") means nothing to the
      // operator; what they can act on is whether the computer answered.
      setError('Couldn’t load this canvas from your computer. Check that Consistent CEO is open on it.');
    }
  }, [entityParam]);

  useEffect(() => {
    if (demo === false) load();
  }, [load, demo]);

  const pan = useMemo(
    () =>
      Gesture.Pan()
        .maxPointers(1)
        .onStart(() => {
          savedTx.value = tx.value;
          savedTy.value = ty.value;
        })
        .onUpdate((e) => {
          tx.value = savedTx.value + e.translationX;
          ty.value = savedTy.value + e.translationY;
        }),
    [savedTx, savedTy, tx, ty],
  );

  const pinch = useMemo(
    () =>
      Gesture.Pinch()
        .onStart(() => {
          savedScale.value = scale.value;
          savedTx.value = tx.value;
          savedTy.value = ty.value;
        })
        .onUpdate((e) => {
          const next = Math.min(MAX_SCALE, Math.max(MIN_SCALE, savedScale.value * e.scale));
          const ratio = next / savedScale.value;
          // Zoom around the screen centre so the map doesn't lurch.
          tx.value = screenW / 2 - (screenW / 2 - savedTx.value) * ratio;
          ty.value = screenH / 2 - (screenH / 2 - savedTy.value) * ratio;
          scale.value = next;
        }),
    [savedScale, savedTx, savedTy, scale, screenH, screenW, tx, ty],
  );

  const viewport = useMemo(() => Gesture.Simultaneous(pan, pinch), [pan, pinch]);

  const worldStyle = useAnimatedStyle(() => ({
    position: 'absolute',
    width: WORLD,
    height: WORLD,
    transform: [{ translateX: tx.value }, { translateY: ty.value }, { scale: scale.value }],
    transformOrigin: 'top left',
  }));

  const nodes = data?.nodes ?? [];
  const edges = data?.edges ?? [];
  const nodeById = useMemo(() => new Map(nodes.map((n) => [n.id, n])), [nodes]);
  const selected = selectedId ? (nodeById.get(selectedId) ?? null) : null;

  useEffect(() => {
    setLabelDraft(selected?.label ?? '');
  }, [selectedId, selected?.label]);

  const handleMoved = useCallback(
    (id: string, x: number, y: number) => {
      setData((d) =>
        d
          ? { ...d, nodes: d.nodes.map((n) => (n.id === id ? { ...n, x, y } : n)) }
          : d,
      );
      updateCanvasNode(id, { x, y }).catch(() => load());
    },
    [load],
  );

  const handleSelect = useCallback(
    (id: string) => {
      haptics.tap();
      if (linkFrom && linkFrom !== id) {
        // Second tap of link mode → create the edge.
        createCanvasEdge(linkFrom, id)
          .then((edge) => {
            haptics.success();
            setData((d) => (d ? { ...d, edges: [...d.edges, edge] } : d));
          })
          .catch(() => load())
          .finally(() => setLinkFrom(null));
        return;
      }
      setSelectedId((cur) => (cur === id ? null : id));
    },
    [linkFrom, load],
  );

  const addNote = useCallback(async () => {
    haptics.press();
    // Drop the note at the current viewport centre, slightly jittered so
    // repeated adds don't stack invisibly.
    const worldX = (screenW / 2 - tx.value) / scale.value - ORIGIN + (Math.random() - 0.5) * 40;
    const worldY = (screenH / 2 - ty.value) / scale.value - ORIGIN + (Math.random() - 0.5) * 40;
    try {
      const node = await createCanvasNode({
        entityId: entityParam,
        kind: 'note',
        label: 'New note',
        x: Math.round(worldX),
        y: Math.round(worldY),
      });
      setData((d) => (d ? { ...d, nodes: [...d.nodes, node] } : d));
      setSelectedId(node.id);
    } catch {
      load();
    }
  }, [entityParam, load, scale, screenH, screenW, tx, ty]);

  const removeSelected = useCallback(async () => {
    if (!selectedId) return;
    haptics.warning();
    const id = selectedId;
    setSelectedId(null);
    setData((d) =>
      d
        ? {
            ...d,
            nodes: d.nodes.filter((n) => n.id !== id),
            edges: d.edges.filter((e) => e.fromNode !== id && e.toNode !== id),
          }
        : d,
    );
    deleteCanvasNode(id).catch(() => load());
  }, [load, selectedId]);

  const saveLabel = useCallback(() => {
    if (!selectedId) return;
    const label = labelDraft.trim();
    setData((d) =>
      d
        ? { ...d, nodes: d.nodes.map((n) => (n.id === selectedId ? { ...n, label } : n)) }
        : d,
    );
    updateCanvasNode(selectedId, { label }).catch(() => load());
  }, [labelDraft, load, selectedId]);

  const removeEdge = useCallback(
    (id: string) => {
      setData((d) => (d ? { ...d, edges: d.edges.filter((e) => e.id !== id) } : d));
      deleteCanvasEdge(id).catch(() => load());
    },
    [load],
  );

  /** File the selected note under a real entity: find-or-create the live
      entity node on this canvas, then draw the edge note → entity. */
  const attachToEntity = useCallback(
    async (entityRefId: string | undefined) => {
      setEntityPickerOpen(false);
      if (!entityRefId || !selectedId || !data) return;
      const note = data.nodes.find((n) => n.id === selectedId);
      if (!note) return;
      try {
        let target = data.nodes.find((n) => n.kind === 'entity' && n.refId === entityRefId);
        let nodes = data.nodes;
        if (!target) {
          target = await createCanvasNode({
            entityId: entityParam,
            kind: 'entity',
            refId: entityRefId,
            x: Math.round(note.x + 230),
            y: Math.round(note.y + 60),
          });
          nodes = [...nodes, target];
        }
        const edge = await createCanvasEdge(note.id, target.id);
        haptics.success();
        setData((d) => (d ? { ...d, nodes, edges: [...d.edges, edge] } : d));
      } catch {
        load();
      }
    },
    [data, entityParam, load, selectedId],
  );

  const isPersonal = data?.entityId.startsWith('personal_') ?? !entityParam;
  const headerTop = insets.top + spacing[2];

  return (
    <View style={{ flex: 1, backgroundColor: BG }}>
      <StatusBar style="light" />

      <GestureDetector gesture={viewport}>
        <View style={{ position: 'absolute', top: 0, left: 0, right: 0, bottom: 0 }}>
          <Animated.View style={worldStyle}>
            {/* Edges under nodes, in the same transformed plane. Plain Views
                (a thin rotated bar per edge) rather than a world-sized Skia
                canvas — a WORLD×WORLD Metal texture exceeds device limits. */}
            {edges.map((e) => {
              const a = nodeById.get(e.fromNode);
              const b = nodeById.get(e.toNode);
              if (!a || !b) return null;
              const dx = b.x - a.x;
              const dy = b.y - a.y;
              const len = Math.hypot(dx, dy);
              if (len < 1) return null;
              const angle = Math.atan2(dy, dx);
              return (
                <View
                  key={e.id}
                  pointerEvents="none"
                  style={{
                    position: 'absolute',
                    left: ORIGIN + (a.x + b.x) / 2 - len / 2,
                    top: ORIGIN + (a.y + b.y) / 2 - 1,
                    width: len,
                    height: 2,
                    backgroundColor: EDGE_COLOR,
                    transform: [{ rotate: `${angle}rad` }],
                  }}
                />
              );
            })}
            {nodes.map((n) => (
              <NodeView
                key={n.id}
                node={n}
                selected={n.id === selectedId}
                linkArmed={n.id === linkFrom}
                scale={scale}
                onSelect={handleSelect}
                onMoved={handleMoved}
              />
            ))}
          </Animated.View>
        </View>
      </GestureDetector>

      {/* Header */}
      <View
        pointerEvents="box-none"
        style={{
          position: 'absolute',
          top: headerTop,
          left: 0,
          right: 0,
          paddingHorizontal: spacing[4],
          flexDirection: 'row',
          alignItems: 'center',
          gap: spacing[3],
        }}
      >
        <Pressable
          onPress={() => router.back()}
          hitSlop={8}
          accessibilityLabel="Back"
          style={{
            width: 38,
            height: 38,
            borderRadius: 19,
            alignItems: 'center',
            justifyContent: 'center',
            backgroundColor: OVERLAY_BG,
            borderWidth: 1,
            borderColor: OVERLAY_BORDER,
          }}
        >
          <Txt style={{ color: sky.text, fontSize: 16 }}>←</Txt>
        </Pressable>
        <View style={{ flex: 1 }}>
          <Txt variant="heading" style={{ color: sky.text }}>
            {isPersonal ? 'Personal canvas' : 'Canvas'}
          </Txt>
          <Txt variant="caption" style={{ color: sky.textSecondary }}>
            {nodes.length} objects · {isPersonal ? 'private to you' : data?.entityId ?? ''}
          </Txt>
        </View>
        {linkFrom ? (
          <Pressable
            onPress={() => setLinkFrom(null)}
            style={{
              paddingHorizontal: spacing[3],
              height: 38,
              borderRadius: radius.full,
              alignItems: 'center',
              justifyContent: 'center',
              backgroundColor: alpha(ACCENT, 0.24),
              borderWidth: 1,
              borderColor: alpha(ACCENT, 0.6),
            }}
          >
            <Txt variant="label" style={{ color: sky.accentText }}>
              Tap a target… ✕
            </Txt>
          </Pressable>
        ) : null}
      </View>

      {/* Demo: nothing to load */}
      {demo ? (
        <View style={{ position: 'absolute', top: 0, left: 0, right: 0, bottom: 0, justifyContent: 'center', paddingHorizontal: spacing[5] }}>
          <NeedsComputer
            onSky
            icon="▦"
            title="Your canvas lives on your computer"
            body="Notes, tasks and missions you arrange here are saved by Consistent CEO on your computer, and stay in step with their live status. Pair this phone to open your canvas."
          />
        </View>
      ) : null}

      {/* Error / loading */}
      {!data && demo === false && (
        <View style={{ position: 'absolute', top: 0, left: 0, right: 0, bottom: 0, alignItems: 'center', justifyContent: 'center', gap: spacing[3] }}>
          {error ? (
            <>
              <Txt style={{ color: sky.textSecondary }}>{error}</Txt>
              <Pressable
                onPress={load}
                style={{ paddingHorizontal: spacing[4], paddingVertical: spacing[2], borderRadius: radius.full, backgroundColor: alpha(ACCENT, 0.24), borderWidth: 1, borderColor: alpha(ACCENT, 0.6) }}
              >
                <Txt variant="label" style={{ color: sky.accentText }}>Retry</Txt>
              </Pressable>
            </>
          ) : (
            <ActivityIndicator color={ACCENT} />
          )}
        </View>
      )}

      {/* Selection card */}
      {selected ? (
        <Animated.View
          entering={SlideInDown.duration(240)}
          exiting={FadeOut.duration(140)}
          style={{ position: 'absolute', left: spacing[4], right: spacing[4], bottom: insets.bottom + 96 }}
        >
          <View
            style={{
              borderRadius: radius.xl,
              backgroundColor: CARD_BG,
              borderWidth: 1,
              borderColor: OVERLAY_BORDER,
              padding: spacing[4],
              gap: spacing[3],
            }}
          >
            <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2] }}>
              <View style={{ width: 10, height: 10, borderRadius: 5, backgroundColor: selected.color ?? KIND_ACCENT[selected.kind] ?? sky.textSecondary }} />
              <Txt variant="label" style={{ color: sky.textMuted, letterSpacing: 1 }}>
                {selected.kind.toUpperCase()}
                {selected.live ? ` · ${selected.live.exists ? selected.live.status : 'deleted'}` : ''}
              </Txt>
              <Pressable onPress={() => setSelectedId(null)} hitSlop={8} style={{ marginLeft: 'auto' }} accessibilityLabel="Close">
                <Txt style={{ color: sky.textSecondary, fontSize: 13 }}>✕</Txt>
              </Pressable>
            </View>

            {selected.live && selected.live.exists ? (
              <Txt style={{ color: sky.text, fontSize: 15 }}>{selected.live.title}</Txt>
            ) : (
              <TextInput
                value={labelDraft}
                onChangeText={setLabelDraft}
                onBlur={saveLabel}
                onSubmitEditing={saveLabel}
                placeholder="Label…"
                placeholderTextColor="#6d7280"
                style={{
                  color: sky.text,
                  fontSize: 15,
                  backgroundColor: sky.control,
                  borderRadius: radius.md,
                  paddingHorizontal: spacing[3],
                  paddingVertical: spacing[2],
                }}
              />
            )}

            {/* Edges from this node, deletable. */}
            {edges.filter((e) => e.fromNode === selected.id || e.toNode === selected.id).map((e) => {
              const otherId = e.fromNode === selected.id ? e.toNode : e.fromNode;
              const other = nodeById.get(otherId);
              return (
                <View key={e.id} style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2] }}>
                  <Txt variant="caption" style={{ color: sky.textSecondary, flex: 1 }} numberOfLines={1}>
                    ⇄ {other?.live?.title || other?.label || other?.kind || otherId}
                  </Txt>
                  <Pressable onPress={() => removeEdge(e.id)} hitSlop={6} accessibilityLabel="Remove link">
                    <Txt variant="caption" style={{ color: sky.textSecondary }}>✕</Txt>
                  </Pressable>
                </View>
              );
            })}

            <View style={{ flexDirection: 'row', gap: spacing[2] }}>
              {selected.kind !== 'entity' ? (
                <Pressable
                  onPress={() => {
                    haptics.press();
                    setEntityPickerOpen(true);
                  }}
                  style={{
                    flex: 1,
                    height: 40,
                    borderRadius: radius.md,
                    alignItems: 'center',
                    justifyContent: 'center',
                    backgroundColor: alpha(palette.smoke, 0.18),
                    borderWidth: 1,
                    borderColor: alpha(palette.smoke, 0.5),
                  }}
                  accessibilityRole="button"
                  accessibilityLabel="Attach to an entity"
                >
                  <Txt variant="label" style={{ color: sky.text }}>⬡ Entity</Txt>
                </Pressable>
              ) : null}
              <Pressable
                onPress={() => {
                  haptics.press();
                  setLinkFrom(selected.id);
                  setSelectedId(null);
                }}
                style={{ flex: 1, height: 40, borderRadius: radius.md, alignItems: 'center', justifyContent: 'center', backgroundColor: alpha(ACCENT, 0.2), borderWidth: 1, borderColor: alpha(ACCENT, 0.5) }}
                accessibilityRole="button"
                accessibilityLabel="Link to another object"
              >
                <Txt variant="label" style={{ color: sky.accentText }}>Link →</Txt>
              </Pressable>
              <Pressable
                onPress={removeSelected}
                style={{ height: 40, paddingHorizontal: spacing[4], borderRadius: radius.md, alignItems: 'center', justifyContent: 'center', backgroundColor: alpha(palette.failed, 0.14), borderWidth: 1, borderColor: alpha(palette.failed, 0.4) }}
                accessibilityRole="button"
                accessibilityLabel="Delete object"
              >
                <Txt variant="label" style={{ color: palette.failed }}>Delete</Txt>
              </Pressable>
            </View>
          </View>
        </Animated.View>
      ) : null}

      {/* Bottom toolbar */}
      {demo === false ? (
        <Animated.View
          entering={FadeIn}
          pointerEvents="box-none"
          style={{
            position: 'absolute',
            left: 0,
            right: 0,
            bottom: insets.bottom + spacing[4],
            flexDirection: 'row',
            justifyContent: 'center',
            gap: spacing[3],
          }}
        >
          <Pressable
            onPress={addNote}
            accessibilityRole="button"
            accessibilityLabel="Add note"
            style={{
              height: 48,
              paddingHorizontal: spacing[5],
              borderRadius: radius.full,
              alignItems: 'center',
              justifyContent: 'center',
              flexDirection: 'row',
              gap: 6,
              backgroundColor: ACCENT,
            }}
          >
            <Txt style={{ color: palette.white, fontSize: 16 }} weight="bold">＋</Txt>
            <Txt style={{ color: palette.white, fontSize: 14 }} weight="semibold">Note</Txt>
          </Pressable>
          <Pressable
            onPress={load}
            accessibilityRole="button"
            accessibilityLabel="Refresh"
            style={{
              width: 48,
              height: 48,
              borderRadius: 24,
              alignItems: 'center',
              justifyContent: 'center',
              backgroundColor: OVERLAY_BG,
              borderWidth: 1,
              borderColor: OVERLAY_BORDER,
            }}
          >
            <Txt style={{ color: sky.textSecondary, fontSize: 16 }}>⟳</Txt>
          </Pressable>
        </Animated.View>
      ) : null}

      <EntityPickerSheet
        visible={entityPickerOpen}
        entities={appEntities}
        onSelect={(id) => attachToEntity(id)}
        onClose={() => setEntityPickerOpen(false)}
      />
    </View>
  );
}
