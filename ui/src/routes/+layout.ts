// SvelteKit needs this to know we're a fully prerendered SPA — Tauri serves
// the build output as static files and routes client-side.
export const prerender = true;
export const ssr = false;
