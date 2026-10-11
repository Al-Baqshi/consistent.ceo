// Default Expo Metro config. Skia + Reanimated 4 work with the stock resolver;
// kept explicit so future asset/transformer tweaks have a home.
const { getDefaultConfig } = require('expo/metro-config');

const config = getDefaultConfig(__dirname);

module.exports = config;
