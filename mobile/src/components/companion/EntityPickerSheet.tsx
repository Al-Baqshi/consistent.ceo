import React from 'react';
import { Modal, Pressable, ScrollView, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { haptics } from '@/lib/haptics';
import { useTheme } from '@/theme/ThemeProvider';
import { entityColor, radius, spacing } from '@/theme/tokens';
import type { Entity } from '@/api';

import { EntityNode } from '../EntityNode';
import { Txt } from '../Txt';

interface Props {
  visible: boolean;
  entities: Entity[];
  selectedId?: string;
  onSelect: (id: string | undefined) => void;
  onClose: () => void;
}

/** Bottom sheet: choose which entity the conversation/capture is about. */
export function EntityPickerSheet({ visible, entities, selectedId, onSelect, onClose }: Props) {
  const theme = useTheme();
  const insets = useSafeAreaInsets();

  const Row = ({
    label,
    caption,
    active,
    orb,
    onPress,
  }: {
    label: string;
    caption: string;
    active: boolean;
    orb?: React.ReactNode;
    onPress: () => void;
  }) => (
    <Pressable
      onPress={() => {
        haptics.tap();
        onPress();
      }}
      style={{
        flexDirection: 'row',
        alignItems: 'center',
        gap: spacing[3],
        paddingVertical: spacing[3],
        paddingHorizontal: spacing[4],
        backgroundColor: active ? theme.colors.bgSelected : 'transparent',
        borderRadius: radius.lg,
      }}
    >
      {orb ?? (
        <View
          style={{
            width: 34,
            height: 34,
            borderRadius: 17,
            alignItems: 'center',
            justifyContent: 'center',
            backgroundColor: theme.colors.bgCardHover,
          }}
        >
          <Txt tone="accent" style={{ fontSize: 15 }}>
            ✷
          </Txt>
        </View>
      )}
      <View style={{ flex: 1 }}>
        <Txt variant="label" tone={active ? 'accent' : 'primary'} style={{ fontSize: 15 }}>
          {label}
        </Txt>
        <Txt variant="caption" tone="muted" numberOfLines={1}>
          {caption}
        </Txt>
      </View>
      {active ? (
        <Txt tone="accent" weight="bold">
          ✓
        </Txt>
      ) : null}
    </Pressable>
  );

  return (
    <Modal transparent visible={visible} animationType="slide" onRequestClose={onClose}>
      <Pressable onPress={onClose} style={{ flex: 1, backgroundColor: theme.colors.bgOverlay, justifyContent: 'flex-end' }}>
        <Pressable
          style={{
            backgroundColor: theme.colors.bgSurface,
            borderTopLeftRadius: radius['2xl'],
            borderTopRightRadius: radius['2xl'],
            borderWidth: 1,
            borderColor: theme.colors.border,
            paddingTop: spacing[3],
            paddingBottom: insets.bottom + spacing[4],
            maxHeight: '70%',
          }}
        >
          <View style={{ alignItems: 'center', paddingBottom: spacing[2] }}>
            <View style={{ width: 38, height: 5, borderRadius: 3, backgroundColor: theme.colors.border }} />
          </View>
          <Txt variant="heading" style={{ paddingHorizontal: spacing[4], paddingBottom: spacing[2] }}>
            Talking about
          </Txt>
          <ScrollView style={{ paddingHorizontal: spacing[2] }}>
            <Row
              label="Whole workspace"
              caption="Everything you can see — no single entity"
              active={!selectedId}
              onPress={() => {
                onSelect(undefined);
                onClose();
              }}
            />
            {entities.map((e) => (
              <Row
                key={e.id}
                label={e.name}
                caption={e.summary}
                active={selectedId === e.id}
                orb={<EntityNode type={e.type} color={entityColor(e)} size={34} glowing={false} />}
                onPress={() => {
                  onSelect(e.id);
                  onClose();
                }}
              />
            ))}
          </ScrollView>
        </Pressable>
      </Pressable>
    </Modal>
  );
}
