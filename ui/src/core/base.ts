// Where the UI lives. The local daemon serves it at `/`. The hosted service
// (agentvc-cloud) serves one machine's space under `/s/<slug>/`, with that
// space's API at `/s/<slug>/api`, relayed to the daemon on that machine.

const m = /^\/s\/([A-Za-z0-9_-]+)(?=\/|$)/.exec(window.location.pathname)

/** `''` locally, `/s/<slug>` when hosted. No trailing slash. */
export const basePath: string = m ? `/s/${m[1]}` : ''

/** True when the UI is served by the hosted service. */
export const hosted: boolean = m !== null

/** URL of an API path such as `/tasks`. */
export const apiUrl = (path: string): string => `${basePath}/api${path}`
