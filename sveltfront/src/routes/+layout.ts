// SPA mode: the dashboard is a pure client app talking to the local gateway.
// Disable SSR and prerendering so adapter-static emits a single index.html
// shell (no build-time data fetching, no browser-API access during build).
export const ssr = false;
export const prerender = false;
