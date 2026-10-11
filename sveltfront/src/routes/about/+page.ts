// The Knowledge Hub is the one route that exists to be read by people and crawlers
// rather than by an authenticated operator, so it opts out of the app-wide SPA mode
// (`ssr = false` in the root `+layout.ts`) and is prerendered to real HTML at build
// time. adapter-static emits it as `build/about.html`, which means search engines and
// LLM crawlers get the content without executing any JavaScript.
export const prerender = true;
export const ssr = true;
