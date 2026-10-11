import {
  BlurMask,
  Canvas,
  Circle,
  Fill,
  Group,
  Line,
  RadialGradient,
  Rect,
  Shader,
  Skia,
  useClock,
  vec,
  LinearGradient as SkLinearGradient,
} from '@shopify/react-native-skia';
import React, { useMemo, useState } from 'react';
import { LayoutChangeEvent, Pressable, View } from 'react-native';
import { Gesture, GestureDetector } from 'react-native-gesture-handler';
import Animated, {
  cancelAnimation,
  useAnimatedStyle,
  useDerivedValue,
  useSharedValue,
  withDecay,
  withTiming,
  type SharedValue,
} from 'react-native-reanimated';

import { alpha, mix } from '@/lib/color';
import { haptics } from '@/lib/haptics';
import { entityColor, entityGlyph, sky } from '@/theme/tokens';
import type { Entity, EntityType } from '@/api';

import { Txt } from './Txt';

/**
 * The Brain map v4 — a living game-like night sky.
 *
 * Rendering:
 * - A real SkSL fragment shader paints the sky: flowing fractal nebulas and
 *   a twinkling starfield, animated on the GPU via a Skia clock (no React
 *   re-renders, no worklet-created Skia objects).
 * - Planets are painted in-canvas as lit spheres (offset key light, rim,
 *   specular) with breathing atmosphere glows and a gentle orbital bob,
 *   each on its own phase.
 * - Energy pulses travel along the selected planet's relation edges.
 * - Touch stays native: invisible Pressables (with crisp RN glyph/labels)
 *   sit exactly over each planet — tap to select, hold-drag to move.
 */

export const MAP_BG = sky.bg;
const MAP_TEXT = sky.textSecondary;
const MAP_TEXT_BRIGHT = sky.text;
const EDGE = 'rgba(214, 222, 235, 0.10)';

const MIN_SCALE = 0.45;
const MAX_SCALE = 3.5;
const GOLDEN = 2.399963;
/** Extra world-canvas bleed so planets aren't clipped at the screen rect. */
const WORLD_PAD = 240;
/** Momentum glide: closer to 1 coasts longer. */
const DECELERATION = 0.994;
/**
 * How far the world may travel from centre before it would leave the screen,
 * as a multiple of the viewport: `viewport * (0.5 * scale + PAN_SLACK)`.
 * Inlined at every call site — worklets can't reach a module-scope helper.
 */
const PAN_SLACK = 0.35;

// ── Sky shader (SkSL) — compiled once on the JS thread ─────────────────
const SKY_SKSL = `
uniform float u_time;
uniform float2 u_res;
uniform float2 u_off;

float hash(float2 p) {
  return fract(sin(dot(p, float2(127.1, 311.7))) * 43758.5453);
}

float noise(float2 p) {
  float2 i = floor(p);
  float2 f = fract(p);
  float2 u = f * f * (3.0 - 2.0 * f);
  float a = hash(i);
  float b = hash(i + float2(1.0, 0.0));
  float c = hash(i + float2(0.0, 1.0));
  float d = hash(i + float2(1.0, 1.0));
  return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

float fbm(float2 p) {
  float v = 0.0;
  float amp = 0.5;
  for (int i = 0; i < 4; i++) {
    v += amp * noise(p);
    p *= 2.03;
    amp *= 0.5;
  }
  return v;
}

float fbm2(float2 p) {
  return 0.5 * noise(p) + 0.25 * noise(p * 2.03);
}

float starLayer(float2 xy, float cell, float t, float density) {
  float2 g = floor(xy / cell);
  float h = hash(g);
  if (h < density) { return 0.0; }
  float2 pos = (g + float2(hash(g + 1.3), hash(g + 2.7))) * cell;
  float d = length(xy - pos);
  float tw = 0.55 + 0.45 * sin(t * (0.8 + h * 2.2) + h * 6.2831);
  return smoothstep(1.6, 0.0, d) * tw;
}

half4 main(float2 xy) {
  float2 uv = xy / u_res;
  float t = u_time;
  // Parallax-shifted coords for star/nebula sampling; the base gradient and
  // vignette stay anchored to the viewport so edges never show.
  float2 sxy = xy + u_off;
  float2 suv = sxy / u_res;

  // Deep-space base, slightly lighter toward the top.
  float3 col = mix(float3(0.012, 0.016, 0.038), float3(0.035, 0.045, 0.085), 1.0 - uv.y);

  // Flowing nebulas (domain-warped fbm, trimmed for mobile GPUs).
  float2 p = suv * float2(u_res.x / u_res.y, 1.0) * 3.0;
  float warp = fbm2(p * 1.6 - t * 0.02) * 0.7;
  float n1 = fbm(p + float2(t * 0.018, -t * 0.012) + warp);
  float n2 = fbm(p * 0.8 + float2(-t * 0.012, t * 0.016) + 3.1);

  col += float3(0.79, 0.54, 0.29) * smoothstep(0.55, 0.95, n1) * 0.12; // copper
  col += float3(0.45, 0.38, 0.62) * smoothstep(0.48, 0.9, n2) * 0.10;  // violet

  // Twinkling stars, two depth layers.
  float s = starLayer(sxy, 42.0, t, 0.35) * 0.85 + starLayer(sxy, 90.0, t * 0.7, 0.55) * 1.0;
  col += float3(0.92, 0.95, 1.0) * s;

  // Soft vignette.
  float d = distance(uv, float2(0.5, 0.44));
  col *= 1.0 - d * 0.25;

  return half4(half3(col), 1.0);
}
`;

const SKY_EFFECT = Skia.RuntimeEffect.Make(SKY_SKSL);

export interface NodeOffset {
  x: number;
  y: number;
}

interface LayoutNode {
  id: string;
  name: string;
  type: EntityType;
  color: string;
  x: number;
  y: number;
  r: number;
  pinned: boolean;
  /** Stable per-node animation phase. */
  phase: number;
}

interface Props {
  entities: Entity[];
  selectedId: string | null;
  onSelectNode: (id: string | null) => void;
  nodeOffsets?: Record<string, NodeOffset>;
  onNodeMove?: (id: string, pos: NodeOffset) => void;
  controlsTopOffset?: number;
}

function srand(i: number): number {
  const x = Math.sin(i * 127.1 + 311.7) * 43758.5453;
  return x - Math.floor(x);
}

function baseLayout(entities: Entity[], w: number, h: number): LayoutNode[] {
  const cx = w / 2;
  const cy = h / 2;
  const sorted = [...entities].sort((a, b) => b.weight - a.weight);
  // A golden-angle spiral packs tighter as the count grows; widen the rings so
  // a large world stays readable (zoom out to see it whole).
  const spread = 1 + Math.max(0, sorted.length - 14) * 0.02;
  return sorted.map((e, i) => {
    const r = 15 + e.weight * 21;
    const base = {
      id: e.id,
      name: e.name,
      type: e.type,
      color: entityColor(e),
      r,
      pinned: e.pinned,
      phase: srand(i + 17) * Math.PI * 2,
    };
    if (i === 0) return { ...base, x: cx, y: cy };
    const angle = i * GOLDEN;
    const dist = (74 + 46 * Math.sqrt(i)) * spread;
    return { ...base, x: cx + Math.cos(angle) * dist, y: cy + Math.sin(angle) * dist };
  });
}

export function BrainMap({
  entities,
  selectedId,
  onSelectNode,
  nodeOffsets,
  onNodeMove,
  controlsTopOffset,
}: Props) {
  const [size, setSize] = useState({ w: 0, h: 0 });
  const [drag, setDrag] = useState<{ id: string; x: number; y: number } | null>(null);

  const clock = useClock();

  const tx = useSharedValue(0);
  const ty = useSharedValue(0);
  const scale = useSharedValue(1);
  const startTx = useSharedValue(0);
  const startTy = useSharedValue(0);
  const startScale = useSharedValue(1);
  // Scale at the previous pinch frame — focal zoom is applied incrementally.
  const prevScale = useSharedValue(1);
  // Viewport size mirrored into shared values so gesture worklets can clamp.
  const vw = useSharedValue(0);
  const vh = useSharedValue(0);

  // Two-stage memo: base positions keep stable object identity so that
  // during a drag only the dragged node's subtree re-renders (React.memo).
  const baseNodes = useMemo(() => {
    if (size.w === 0) return [];
    return baseLayout(entities, size.w, size.h).map((n) => {
      const off = nodeOffsets?.[n.id];
      return off ? { ...n, x: off.x, y: off.y } : n;
    });
  }, [entities, size, nodeOffsets]);

  const nodes = useMemo(
    () =>
      drag
        ? baseNodes.map((n) => (n.id === drag.id ? { ...n, x: drag.x, y: drag.y } : n))
        : baseNodes,
    [baseNodes, drag],
  );

  const edges = useMemo(() => {
    const byId = new Map(nodes.map((n) => [n.id, n]));
    const seen = new Set<string>();
    const list: { key: string; x1: number; y1: number; x2: number; y2: number; active: boolean }[] = [];
    for (const e of entities) {
      const from = byId.get(e.id);
      if (!from) continue;
      for (const rid of e.relatedIds) {
        const to = byId.get(rid);
        if (!to) continue;
        const key = [e.id, rid].sort().join('|');
        if (seen.has(key)) continue;
        seen.add(key);
        list.push({
          key,
          x1: from.x,
          y1: from.y,
          x2: to.x,
          y2: to.y,
          active: selectedId != null && (e.id === selectedId || rid === selectedId),
        });
      }
    }
    return list;
  }, [nodes, entities, selectedId]);

  // Shader uniforms — plain numbers only (no Skia objects in worklets).
  // The sky is a fixed backdrop; u_off adds a gentle parallax as the world pans.
  const uniforms = useDerivedValue(
    () => ({
      u_time: clock.value / 1000,
      u_res: [size.w || 1, size.h || 1],
      u_off: [-tx.value * 0.18, -ty.value * 0.18],
    }),
    [size],
  );

  const zoomTo = (target: number) => {
    cancelAnimation(tx);
    cancelAnimation(ty);
    scale.value = withTiming(target, { duration: 260 });
    if (target === 1) {
      tx.value = withTiming(0, { duration: 260 });
      ty.value = withTiming(0, { duration: 260 });
    }
  };

  // ── Map gestures — memoized so re-renders don't rebuild live handlers ──
  const composed = useMemo(() => {
    const pan = Gesture.Pan()
      .averageTouches(true)
      .maxPointers(2)
      .onStart(() => {
        // A new touch takes over from any in-flight momentum.
        cancelAnimation(tx);
        cancelAnimation(ty);
        startTx.value = tx.value;
        startTy.value = ty.value;
      })
      .onUpdate((e) => {
        // Clamp so the planet field can't be flung entirely off-screen.
        const maxX = vw.value * (0.5 * scale.value + PAN_SLACK);
        const maxY = vh.value * (0.5 * scale.value + PAN_SLACK);
        tx.value = Math.min(maxX, Math.max(-maxX, startTx.value + e.translationX));
        ty.value = Math.min(maxY, Math.max(-maxY, startTy.value + e.translationY));
      })
      .onEnd((e) => {
        // Momentum: flick the map and let it glide to a stop, bounded so it
        // can never coast off into empty space.
        const maxX = vw.value * (0.5 * scale.value + PAN_SLACK);
        const maxY = vh.value * (0.5 * scale.value + PAN_SLACK);
        tx.value = withDecay({
          velocity: e.velocityX,
          deceleration: DECELERATION,
          clamp: [-maxX, maxX],
        });
        ty.value = withDecay({
          velocity: e.velocityY,
          deceleration: DECELERATION,
          clamp: [-maxY, maxY],
        });
      });

    const pinch = Gesture.Pinch()
      .onStart(() => {
        cancelAnimation(tx);
        cancelAnimation(ty);
        startScale.value = scale.value;
        prevScale.value = scale.value;
      })
      .onUpdate((e) => {
        const next = Math.min(MAX_SCALE, Math.max(MIN_SCALE, startScale.value * e.scale));
        const k = next / prevScale.value;
        if (k !== 1) {
          // Zoom toward the fingers, not the screen centre: keep the focal
          // point pinned while the world grows around it. Transforms are
          // centre-relative, hence the half-viewport offset.
          const fx = e.focalX - vw.value / 2;
          const fy = e.focalY - vh.value / 2;
          tx.value = tx.value * k + fx * (1 - k);
          ty.value = ty.value * k + fy * (1 - k);
          // Keep the pan gesture's origin in sync — it recomputes from
          // startTx/startTy absolutely, and would otherwise undo this.
          startTx.value = startTx.value * k + fx * (1 - k);
          startTy.value = startTy.value * k + fy * (1 - k);
        }
        scale.value = next;
        prevScale.value = next;
      });

    const doubleTap = Gesture.Tap()
      .numberOfTaps(2)
      .runOnJS(true)
      .onEnd((e) => {
        haptics.tap();
        const zoomedIn = scale.value > 1.2;
        const target = zoomedIn ? 1 : 1.9;
        if (zoomedIn) {
          scale.value = withTiming(target, { duration: 260 });
          tx.value = withTiming(0, { duration: 260 });
          ty.value = withTiming(0, { duration: 260 });
          return;
        }
        // Zoom in toward whatever was tapped, so double-tapping a distant
        // planet brings it to you instead of the middle of the map.
        const k = target / scale.value;
        const fx = e.x - vw.value / 2;
        const fy = e.y - vh.value / 2;
        const maxX = vw.value * (0.5 * target + PAN_SLACK);
        const maxY = vh.value * (0.5 * target + PAN_SLACK);
        const nx = Math.min(maxX, Math.max(-maxX, tx.value * k + fx * (1 - k)));
        const ny = Math.min(maxY, Math.max(-maxY, ty.value * k + fy * (1 - k)));
        scale.value = withTiming(target, { duration: 260 });
        tx.value = withTiming(nx, { duration: 260 });
        ty.value = withTiming(ny, { duration: 260 });
      });

    return Gesture.Simultaneous(pan, pinch, doubleTap);
    // Shared values are stable references.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const worldStyle = useAnimatedStyle(() => ({
    transform: [
      { translateX: tx.value },
      { translateY: ty.value },
      { scale: scale.value },
    ],
  }));

  const onLayout = (e: LayoutChangeEvent) => {
    const { width, height } = e.nativeEvent.layout;
    vw.value = width;
    vh.value = height;
    if (width !== size.w || height !== size.h) setSize({ w: width, h: height });
  };

  const zoomBy = (f: number) => {
    haptics.tap();
    cancelAnimation(tx);
    cancelAnimation(ty);
    scale.value = withTiming(Math.min(MAX_SCALE, Math.max(MIN_SCALE, scale.value * f)), {
      duration: 200,
    });
  };

  return (
    <View style={{ flex: 1, backgroundColor: MAP_BG, overflow: 'hidden' }} onLayout={onLayout}>
      <GestureDetector gesture={composed}>
        <View style={{ flex: 1 }}>
          {/* Living sky — a fixed full-bleed backdrop that never pans or
              scales (only a soft parallax via u_off), so panning/zooming the
              world can never expose a hard canvas edge. */}
          {size.w > 0 ? (
            <Canvas
              pointerEvents="none"
              style={{ position: 'absolute', top: 0, left: 0, width: size.w, height: size.h }}
            >
              {SKY_EFFECT ? (
                <Fill>
                  <Shader source={SKY_EFFECT} uniforms={uniforms} />
                </Fill>
              ) : (
                <Rect x={0} y={0} width={size.w} height={size.h}>
                  <SkLinearGradient
                    start={vec(0, 0)}
                    end={vec(size.w * 0.4, size.h)}
                    colors={['#0a0d18', '#06070c', '#090a12']}
                  />
                </Rect>
              )}
            </Canvas>
          ) : null}

          {/* Tap empty sky → deselect (fixed, under the world layer) */}
          <Pressable
            style={{ position: 'absolute', top: 0, left: 0, right: 0, bottom: 0 }}
            onPress={() => onSelectNode(null)}
          />

          <Animated.View
            pointerEvents="box-none"
            style={[{ position: 'absolute', top: 0, left: 0, width: size.w, height: size.h }, worldStyle]}
          >
            {size.w > 0 ? (
              <Canvas
                pointerEvents="none"
                style={{
                  position: 'absolute',
                  top: -WORLD_PAD,
                  left: -WORLD_PAD,
                  width: size.w + WORLD_PAD * 2,
                  height: size.h + WORLD_PAD * 2,
                }}
              >
                <Group transform={[{ translateX: WORLD_PAD }, { translateY: WORLD_PAD }]}>
                {/* Relation edges */}
                {edges.map((e) => (
                  <Line
                    key={e.key}
                    p1={vec(e.x1, e.y1)}
                    p2={vec(e.x2, e.y2)}
                    color={e.active ? alpha('#c98a4b', 0.45) : EDGE}
                    strokeWidth={e.active ? 1.4 : 1}
                  />
                ))}

                {/* Energy pulses along active edges */}
                {edges
                  .filter((e) => e.active)
                  .map((e, i) => (
                    <EdgePulse key={`p${e.key}`} edge={e} clock={clock} index={i} />
                  ))}

                {/* Planets — lit spheres with breathing atmospheres */}
                {nodes.map((n) => (
                  <SkPlanet
                    key={n.id}
                    node={n}
                    clock={clock}
                    emphasized={selectedId === n.id || drag?.id === n.id}
                  />
                ))}
                </Group>
              </Canvas>
            ) : null}

            {/* Native touch layer — invisible hit areas + crisp text */}
            {nodes.map((n) => (
              <NodeTouch
                key={n.id}
                node={n}
                selected={selectedId === n.id}
                dragging={drag?.id === n.id}
                mapScale={scale}
                onSelectNode={onSelectNode}
                setDrag={setDrag}
                onNodeMove={onNodeMove}
              />
            ))}
          </Animated.View>
        </View>
      </GestureDetector>

      {/* Zoom controls */}
      <View
        style={
          controlsTopOffset != null
            ? { position: 'absolute', right: 12, top: controlsTopOffset, gap: 6 }
            : { position: 'absolute', right: 12, bottom: 12, gap: 6 }
        }
      >
        <MapButton glyph="＋" label="Zoom in" onPress={() => zoomBy(1.35)} />
        <MapButton glyph="－" label="Zoom out" onPress={() => zoomBy(1 / 1.35)} />
        <MapButton glyph="⌖" label="Recenter" onPress={() => { haptics.tap(); zoomTo(1); }} />
      </View>
    </View>
  );
}

/** A planet painted in Skia: atmosphere glow, lit body, rim + specular. */
const SkPlanet = React.memo(function SkPlanet({
  node,
  clock,
  emphasized,
}: {
  node: LayoutNode;
  clock: SharedValue<number>;
  emphasized: boolean;
}) {
  const { x, y, r, phase, color } = node;

  const hi = useMemo(() => mix(color, '#ffffff', 0.55), [color]);
  const lo = useMemo(() => mix(color, '#000000', 0.45), [color]);
  const rim = useMemo(() => mix(color, '#ffffff', 0.75), [color]);

  // Gentle orbital bob — pure math on the UI thread.
  const bx = useDerivedValue(() => x + Math.sin(clock.value / 1000 * 0.55 + phase) * 3.5, [x, phase]);
  const by = useDerivedValue(() => y + Math.cos(clock.value / 1000 * 0.42 + phase * 1.7) * 2.8, [y, phase]);

  // Breathing atmosphere.
  const glowOpacity = useDerivedValue(
    () => (emphasized ? 0.5 : 0.28) + 0.09 * Math.sin(clock.value / 1000 * 1.4 + phase),
    [emphasized, phase],
  );

  const lightC = useDerivedValue(() => ({ x: bx.value - r * 0.32, y: by.value - r * 0.34 }), [r]);
  const specX = useDerivedValue(() => bx.value - r * 0.38, [r]);
  const specY = useDerivedValue(() => by.value - r * 0.42, [r]);
  const ringOpacity = useDerivedValue(
    () => (emphasized ? 0.75 + 0.2 * Math.sin(clock.value / 1000 * 2.2) : 0),
    [emphasized],
  );

  return (
    <>
      {/* Atmosphere */}
      <Circle cx={bx} cy={by} r={r * 2.05} color={color} opacity={glowOpacity}>
        <BlurMask blur={r * 0.95} style="normal" />
      </Circle>

      {/* Body — offset key light */}
      <Circle cx={bx} cy={by} r={r}>
        <RadialGradient c={lightC} r={r * 1.75} colors={[hi, color, lo]} />
      </Circle>

      {/* Rim light */}
      <Circle cx={bx} cy={by} r={r} style="stroke" strokeWidth={1.2} color={rim} opacity={0.5} />

      {/* Specular gloss */}
      <Circle cx={specX} cy={specY} r={r * 0.3} color="#ffffff" opacity={0.32}>
        <BlurMask blur={r * 0.22} style="normal" />
      </Circle>

      {/* Selection ring (pulses) */}
      <Circle cx={bx} cy={by} r={r + 8} style="stroke" strokeWidth={1.6} color="#ffffff" opacity={ringOpacity} />
    </>
  );
});

/** A glowing dot travelling along an active relation edge. */
function EdgePulse({
  edge,
  clock,
  index,
}: {
  edge: { x1: number; y1: number; x2: number; y2: number };
  clock: SharedValue<number>;
  index: number;
}) {
  const { x1, y1, x2, y2 } = edge;
  const cx = useDerivedValue(() => {
    const t = (clock.value / 1400 + index * 0.35) % 1;
    return x1 + (x2 - x1) * t;
  }, [x1, x2, index]);
  const cy = useDerivedValue(() => {
    const t = (clock.value / 1400 + index * 0.35) % 1;
    return y1 + (y2 - y1) * t;
  }, [y1, y2, index]);

  return (
    <Circle cx={cx} cy={cy} r={2.6} color="#e8b877" opacity={0.9}>
      <BlurMask blur={3} style="normal" />
    </Circle>
  );
}

/** Invisible native hit area + crisp glyph/label/badge for one planet. */
const NodeTouch = React.memo(function NodeTouch({
  node,
  selected,
  dragging,
  mapScale,
  onSelectNode,
  setDrag,
  onNodeMove,
}: {
  node: LayoutNode;
  selected: boolean;
  dragging: boolean;
  mapScale: { value: number };
  onSelectNode: (id: string | null) => void;
  setDrag: React.Dispatch<React.SetStateAction<{ id: string; x: number; y: number } | null>>;
  onNodeMove?: (id: string, pos: NodeOffset) => void;
}) {
  const d = node.r * 2;

  // Drag origin lives in a ref: a plain per-render object would be swapped
  // into the live gesture on each drag re-render, compounding translation
  // and making the planet run away from the finger.
  const start = React.useRef({ x: 0, y: 0 });
  // Keep latest node coords readable from the once-activated gesture.
  const nodeRef = React.useRef(node);
  nodeRef.current = node;

  const dragGesture = useMemo(
    () =>
      Gesture.Pan()
        // Short hold: picking a planet up should feel immediate, while still
        // losing to a map pan that starts as a swipe.
        .activateAfterLongPress(150)
        .runOnJS(true)
        .onStart(() => {
          const n = nodeRef.current;
          start.current = { x: n.x, y: n.y };
          haptics.heavy();
          setDrag({ id: n.id, x: n.x, y: n.y });
        })
        .onUpdate((e) => {
          const s = mapScale.value || 1;
          setDrag({
            id: nodeRef.current.id,
            x: start.current.x + e.translationX / s,
            y: start.current.y + e.translationY / s,
          });
        })
        .onEnd((e) => {
          const s = mapScale.value || 1;
          setDrag(null);
          onNodeMove?.(nodeRef.current.id, {
            x: start.current.x + e.translationX / s,
            y: start.current.y + e.translationY / s,
          });
          haptics.success();
        }),
    // Stable deps: refs, shared value, setState, store action.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  const onPress = () => {
    haptics.tap();
    onSelectNode(selected ? null : node.id);
  };

  return (
    <View
      pointerEvents="box-none"
      style={{
        position: 'absolute',
        left: node.x - 44,
        top: node.y - node.r - 2,
        width: 88,
        alignItems: 'center',
      }}
    >
      <GestureDetector gesture={dragGesture}>
        <Pressable
          onPress={onPress}
          hitSlop={6}
          accessibilityRole="button"
          accessibilityLabel={node.name}
          style={{ width: d, height: d, alignItems: 'center', justifyContent: 'center' }}
        >
          <Txt style={{ fontSize: Math.max(11, d * 0.3), color: 'rgba(10, 10, 14, 0.55)' }} weight="bold">
            {entityGlyph[node.type]}
          </Txt>
          {node.pinned ? (
            <View
              style={{
                position: 'absolute',
                top: -3,
                right: -3,
                width: 15,
                height: 15,
                borderRadius: 8,
                backgroundColor: '#d8c39a',
                borderWidth: 1.5,
                borderColor: MAP_BG,
                alignItems: 'center',
                justifyContent: 'center',
              }}
            >
              <Txt style={{ fontSize: 8, color: '#0c0e13' }} weight="bold">
                ★
              </Txt>
            </View>
          ) : null}
        </Pressable>
      </GestureDetector>
      <Txt
        variant="caption"
        numberOfLines={1}
        style={{
          marginTop: 5,
          maxWidth: 84,
          fontSize: 11,
          color: selected || dragging ? MAP_TEXT_BRIGHT : MAP_TEXT,
        }}
        weight={selected || dragging ? 'semibold' : 'medium'}
      >
        {node.name}
      </Txt>
    </View>
  );
});

function MapButton({ glyph, label, onPress }: { glyph: string; label: string; onPress: () => void }) {
  return (
    <Pressable
      onPress={onPress}
      accessibilityRole="button"
      accessibilityLabel={label}
      style={{
        width: 38,
        height: 38,
        borderRadius: 12,
        alignItems: 'center',
        justifyContent: 'center',
        backgroundColor: 'rgba(14, 16, 22, 0.82)',
        borderWidth: 1,
        borderColor: 'rgba(255, 255, 255, 0.12)',
      }}
    >
      <Txt style={{ fontSize: 16, color: '#c9c6c0' }} weight="semibold">
        {glyph}
      </Txt>
    </Pressable>
  );
}
