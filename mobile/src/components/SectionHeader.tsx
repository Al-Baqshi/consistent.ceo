import React from 'react';
import { View } from 'react-native';

import { spacing } from '@/theme/tokens';

import { Txt } from './Txt';

interface Props {
  title: string;
  action?: React.ReactNode;
  caption?: string;
  style?: object;
}

export function SectionHeader({ title, action, caption, style }: Props) {
  return (
    <View
      style={[
        {
          flexDirection: 'row',
          alignItems: 'center',
          justifyContent: 'space-between',
          marginTop: spacing[6],
          marginBottom: spacing[3],
        },
        style,
      ]}
    >
      <View style={{ flex: 1 }}>
        <Txt variant="caption" tone="muted" weight="bold" style={{ letterSpacing: 1.2, textTransform: 'uppercase' }}>
          {title}
        </Txt>
        {caption ? (
          <Txt variant="caption" tone="muted" style={{ marginTop: 2 }}>
            {caption}
          </Txt>
        ) : null}
      </View>
      {action}
    </View>
  );
}
