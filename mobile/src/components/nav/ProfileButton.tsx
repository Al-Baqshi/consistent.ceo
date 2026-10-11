import { useRouter } from 'expo-router';
import React from 'react';
import { Pressable } from 'react-native';

import { haptics } from '@/lib/haptics';

import { EntityNode } from '../EntityNode';

/**
 * The way in to You → Vault / Settings / Logs. Present in every tab header so
 * account and secrets are always one tap away, not buried behind whichever
 * screen happens to be open.
 */
export function ProfileButton({ size = 36 }: { size?: number }) {
  const router = useRouter();
  return (
    <Pressable
      onPress={() => {
        haptics.tap();
        router.push('/profile');
      }}
      accessibilityRole="button"
      accessibilityLabel="You — account, vault and settings"
      hitSlop={8}
    >
      <EntityNode type="person" size={size} />
    </Pressable>
  );
}
