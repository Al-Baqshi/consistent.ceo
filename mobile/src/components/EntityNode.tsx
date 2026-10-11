import { LinearGradient } from 'expo-linear-gradient';
import React from 'react';
import { View } from 'react-native';

import { alpha, mix } from '@/lib/color';
import { entityAccent, entityGlyph } from '@/theme/tokens';
import type { EntityType } from '@/api';

import { Txt } from './Txt';

interface Props {
  type: EntityType;
  /** The entity's own colour (`Entity.colour`) — wins over the type accent. */
  color?: string;
  size?: number;
  glowing?: boolean;
  label?: string;
}

/**
 * A glowing "planet" orb for an entity — pure RN (gradient + shadow + halo),
 * so it renders reliably everywhere and stays pressable as a normal view.
 * Used in the Brain map, entity hero, related lists, and the pinned strip.
 */
export function EntityNode({ type, color, size = 56, glowing = true, label }: Props) {
  const accent = color ?? entityAccent[type];
  const hi = mix(accent, '#ffffff', 0.45);
  const lo = mix(accent, '#000000', 0.38);
  const orb = size * 0.74;

  return (
    <View style={{ alignItems: 'center' }}>
      <View style={{ width: size, height: size, alignItems: 'center', justifyContent: 'center' }}>
        {glowing ? (
          <View
            pointerEvents="none"
            style={{
              position: 'absolute',
              width: size * 1.35,
              height: size * 1.35,
              borderRadius: size,
              backgroundColor: alpha(accent, 0.16),
            }}
          />
        ) : null}
        <View
          style={{
            shadowColor: accent,
            shadowOpacity: glowing ? 0.55 : 0.25,
            shadowRadius: size * 0.2,
            shadowOffset: { width: 0, height: 2 },
            elevation: glowing ? 8 : 3,
            borderRadius: orb / 2,
          }}
        >
          {/* Clipping wrapper — LinearGradient can ignore borderRadius on
              the new architecture, so the circle is enforced here. */}
          <View
            style={{
              width: orb,
              height: orb,
              borderRadius: orb / 2,
              overflow: 'hidden',
              borderWidth: 1,
              borderColor: 'rgba(255, 255, 255, 0.4)',
            }}
          >
            <LinearGradient
              colors={[hi, accent, lo] as const}
              start={{ x: 0.15, y: 0 }}
              end={{ x: 0.85, y: 1 }}
              style={{ flex: 1, alignItems: 'center', justifyContent: 'center' }}
            >
              <Txt
                style={{ fontSize: Math.max(10, orb * 0.34), color: 'rgba(12, 10, 8, 0.62)' }}
                weight="bold"
              >
                {entityGlyph[type]}
              </Txt>
            </LinearGradient>
          </View>
        </View>
      </View>
      {label ? (
        <Txt
          variant="caption"
          tone="secondary"
          numberOfLines={1}
          center
          style={{ marginTop: 4, maxWidth: size + 28 }}
        >
          {label}
        </Txt>
      ) : null}
    </View>
  );
}
