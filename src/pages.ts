const htmlEscapes: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
  "'": "&#39;",
};

const escape = (value: string): string => value.replace(/[&<>"']/g, (char) => htmlEscapes[char] ?? char);

const patch = (template: string, slots: Record<string, string>): string =>
  template.replace(/\{\{([a-zA-Z]+)\}\}/g, (_, name: string) => slots[name] ?? "");

const securityHeaders = {
  "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
  "x-frame-options": "DENY",
  "referrer-policy": "same-origin",
};

const style = `
:root{color-scheme:light dark;--bg:light-dark(#fff,#000);--fg:light-dark(#000,#fff);--dim:light-dark(#666,#8a8a8a);
--rule:light-dark(#e5e5e5,#262626);--red:light-dark(#e0241b,#ff2a1f);--on-red:light-dark(#fff,#000);
--ok:light-dark(#138a4b,#3ddc84);--warn:light-dark(#9a5b00,#ffb020);--wash:light-dark(#fff6e5,#1a1305);
--well:light-dark(#f2f2f2,#1a1a1a)}
*{box-sizing:border-box}
body{margin:0;padding:40px 64px 80px;background:var(--bg);color:var(--fg);font:15px/1.6 ui-monospace,"SF Mono",Menlo,Consolas,monospace}
header{display:flex;justify-content:space-between;align-items:center;gap:16px;padding-bottom:20px;border-bottom:1px solid var(--rule)}
.mark,h1{font-family:Impact,Haettenschweiler,"Arial Narrow Bold",sans-serif;font-weight:normal}
.mark{font-size:28px;line-height:32px;color:var(--red)}
.label,header small,dt{font-size:12px;line-height:16px;letter-spacing:.12em;text-transform:uppercase;color:var(--dim)}
main{display:flex;flex-direction:column;gap:36px;max-width:800px;padding-top:64px}
hgroup{display:flex;flex-direction:column;gap:12px}
h1{margin:0;font-size:56px;line-height:60px;overflow-wrap:anywhere}
p{margin:0}
.path{display:flex;align-items:center;font-size:13px}
.path span{display:flex;align-items:center;gap:10px;padding:8px 14px;border:1px solid var(--fg)}
.path span.unverified{border:1px dashed var(--warn)}
.path i{width:8px;height:8px;border-radius:50%;background:var(--fg)}
.path .unverified i{background:var(--warn)}
.path i.live{background:var(--ok)}
.path i.starting{background:none;border:1.5px solid var(--dim)}
.path hr{width:72px;margin:0;border:0;border-top:1px dashed var(--dim)}
.path b{width:16px;height:16px;margin-inline:10px;border-radius:50%;background:var(--red);box-shadow:0 0 0 10px var(--red)}
dl{margin:0;border-top:1px solid var(--rule)}
dl div{display:flex;align-items:center;min-height:56px;border-bottom:1px solid var(--rule)}
dt{width:160px;flex-shrink:0}
dd{margin:0;display:flex;flex-wrap:wrap;align-items:center;gap:12px}
.badge{padding:2px 8px;border:1px solid;font-size:11px;letter-spacing:.1em;text-transform:uppercase}
.badge.verified{color:var(--ok)}
.badge.unverified{color:var(--warn)}
.warning{padding:18px 20px;border-left:3px solid var(--warn);background:var(--wash);font-size:14px}
.warning strong{display:block;color:var(--warn);font-weight:500}
form{display:flex;flex-direction:column;align-items:flex-start;gap:14px}
.buttons{display:flex;flex-wrap:wrap;gap:12px}
button{display:flex;justify-content:space-between;gap:24px;padding:18px 24px;border:1px solid var(--fg);background:none;color:var(--fg);font:inherit;font-weight:500;cursor:pointer}
.primary{min-width:200px;border-color:var(--red);background:var(--red);color:var(--on-red)}
.wide{min-width:360px}
.note,.local{font-size:13px;color:var(--dim)}
details,.local{padding-top:24px;border-top:1px dashed var(--rule)}
summary{cursor:pointer;list-style:none}
summary::-webkit-details-marker{display:none}
summary::before{content:"▸ "}
details[open] summary::before{content:"▾ "}
summary span{color:var(--dim)}
ol{display:flex;flex-direction:column;gap:14px;margin:28px 0;padding:0;list-style:none;counter-reset:step}
ol li{display:flex;align-items:center;gap:14px;counter-increment:step}
ol li::before{content:counter(step);display:grid;place-items:center;width:24px;height:24px;flex-shrink:0;border:1px solid var(--dim);border-radius:50%;font-size:12px}
code{padding:2px 8px;background:var(--well)}
.entry{flex-direction:row;flex-wrap:wrap;align-items:stretch;padding-left:38px}
.entry input{width:14ch;padding:14px 16px;border:1px solid var(--dim);background:none;color:var(--fg);font:inherit;font-size:22px;font-weight:500;letter-spacing:.2em;text-transform:uppercase}
.entry input:focus{outline:2px solid var(--red);outline-offset:-1px}
.tools ul{columns:2;margin:20px 0 0;padding:0;list-style:none;line-height:2}
.tools .danger{color:var(--red)}
.status{display:flex;align-items:center;gap:12px}
.status::before{content:"";width:10px;height:10px;border-radius:50%;background:var(--red)}
.status.wait::before{background:var(--warn)}
.detail{max-width:560px;font-size:16px}
.next{display:flex;padding-top:28px;border-top:1px solid var(--rule);overflow-wrap:anywhere}
.next .label{width:160px;flex-shrink:0;padding-top:3px}
@media (max-width:600px){
body{padding:24px 20px 40px}
main{padding-top:40px;gap:28px}
h1{font-size:36px;line-height:40px}
.path{display:none}
dl div,.next{flex-direction:column;align-items:flex-start;gap:6px;padding:14px 0}
.next{padding-top:20px}
form,.buttons{align-items:stretch;width:100%}
button,.wide{width:100%;min-width:0}
.entry{flex-direction:column;padding-left:0}
.entry input{width:100%}
.connect .approve{order:1}
.connect .approve .primary{border-color:var(--rule);background:none;color:var(--fg)}
.tools ul{columns:1}
}
`;

const templates = {
  document: `<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{{title}} · porchlight</title><style>${style}</style><header><span class="mark">porchlight</span><small>{{where}}</small></header>{{main}}`,
  message: `<main><hgroup><p class="{{tone}}">{{status}} · {{label}}</p><h1>{{title}}</h1></hgroup>
<p class="detail">{{detail}}</p>
<p class="next"><span class="label">next</span>{{next}}</p></main>`,
  path: `<nav class="path"><span class="{{clientClass}}"><i></i>{{client}}</span><hr><b></b><hr><span><i class="{{serverState}}"></i>{{server}}</span></nav>`,
  badge: `<span class="badge {{tone}}">{{label}}</span>`,
  ledger: `<dl><div><dt>asked by<dd>{{client}} {{badge}}</div><div><dt>wants<dd>{{server}}{{tools}}</div></dl>`,
  warning: `<p class="warning"><strong>this app's name isn't verified</strong>anyone can call their app “{{client}}”. only continue if you just added this connector yourself and expect to return to {{host}}.</p>`,
  tool: `<li><code>{{name}}</code>`,
  dangerTool: `<li class="danger"><code>{{name}}</code> can run commands or code`,
  toolList: `<details class="tools" open><summary>tools it can use</summary><ul>{{items}}</ul></details>`,
  authorize: `<main class="connect">{{path}}
<hgroup><p class="label">step 1 of 2 · connect</p><h1>connect {{clientTitle}} to {{server}}</h1></hgroup>
{{ledger}}{{warning}}
<form class="approve" action="http://127.0.0.1:{{approvalPort}}/approve"><input type="hidden" name="req" value="{{requestId}}">
<button class="primary wide">approve on this computer <span>→</span></button><p class="note">{{returnsTo}}.</p></form>
<details><summary>on a phone or another computer? <span>use a code instead</span></summary>
<ol><li>on the computer running porchlight, open a terminal<li>run <code>porchlight approve</code><li>enter the code it shows</ol>
<form class="entry" method="post" action="/oauth/complete-code"><input type="hidden" name="req" value="{{requestId}}">
<input id="code" name="code" placeholder="XXXX-XXXX" maxlength="9" autocomplete="one-time-code" autocapitalize="characters" spellcheck="false" aria-label="Code" required>
<button class="primary">connect <span>→</span></button></form></details></main>`,
  approval: `<main>{{path}}
<hgroup><p class="label">step 2 of 2 · approve</p><h1>allow {{clientTitle}} to use {{server}}?</h1></hgroup>
{{ledger}}{{warning}}{{tools}}
<form method="post" action="http://127.0.0.1:{{approvalPort}}/approve">
<input type="hidden" name="req" value="{{requestId}}">
<input type="hidden" name="csrf" value="{{csrf}}">
<div class="buttons"><button class="primary" name="decision" value="allow">allow <span>→</span></button>
<button name="decision" value="deny">deny</button></div>
<p class="note">{{returnsTo}}. undo it anytime with <code>porchlight clients revoke</code>.</p></form>
<p class="local">this page only opens on this computer. the tunnel can't reach it, so nobody else can press allow.</p></main>`,
} as const;

const render = (title: string, where: string, main: string): string =>
  patch(templates.document, { title: escape(title), where: escape(where), main });

export const htmlResponse = (html: string, status = 200): Response =>
  new Response(html, { status, headers: { "content-type": "text/html; charset=utf-8", ...securityHeaders } });

export const redirectTo = (target: string, params: Record<string, string>): Response => {
  const url = new URL(target);
  for (const [name, value] of Object.entries(params)) if (value) url.searchParams.set(name, value);
  return new Response(null, { status: 302, headers: { location: url.toString(), ...securityHeaders } });
};

export interface Message {
  label: string;
  title: string;
  detail: string;
  next: string;
}

const waitingStatuses = new Set([400, 429]);

export const messageResponse = (status: number, message: Message): Response =>
  htmlResponse(
    render(
      message.title,
      "",
      patch(templates.message, {
        tone: waitingStatuses.has(status) ? "status label wait" : "status label",
        status: String(status),
        label: escape(message.label),
        title: escape(message.title),
        detail: escape(message.detail),
        next: escape(message.next),
      }),
    ),
    status,
  );

export interface ClientIdentity {
  name: string;
  verifiedHost: string | undefined;
}

export interface RequestSummary {
  client: ClientIdentity;
  serverName: string;
  toolCount: number | undefined;
  redirectUri: string;
}

const isLoopback = (hostname: string): boolean =>
  hostname === "localhost" || hostname === "127.0.0.1" || hostname === "[::1]";

const returnHost = (summary: RequestSummary): string => new URL(summary.redirectUri).hostname;

const clientTitle = (client: ClientIdentity): string =>
  client.verifiedHost ? escape(client.name) : `“${escape(client.name)}”`;

const connection = (summary: RequestSummary) => {
  const { client } = summary;
  const host = escape(returnHost(summary));
  const badge = client.verifiedHost
    ? patch(templates.badge, { tone: "verified", label: `✓ verified · ${escape(client.verifiedHost)}` })
    : patch(templates.badge, { tone: "unverified", label: "name not verified" });
  const tools =
    summary.toolCount === undefined ? " — still starting, tools appear once it's up" : ` — ${summary.toolCount} tools`;
  return {
    path: patch(templates.path, {
      clientClass: client.verifiedHost ? "" : "unverified",
      client: escape(client.name),
      serverState: summary.toolCount === undefined ? "starting" : "live",
      server: escape(summary.serverName),
    }),
    ledger: patch(templates.ledger, {
      client: escape(client.name),
      badge,
      server: escape(summary.serverName),
      tools,
    }),
    warning: client.verifiedHost ? "" : patch(templates.warning, { client: escape(client.name), host }),
    clientTitle: clientTitle(client),
    server: escape(summary.serverName),
  };
};

const returnsTo = (summary: RequestSummary, verb: string): string =>
  isLoopback(returnHost(summary))
    ? `${verb} returns to an app on the device that started this. only continue if that was you`
    : `${verb} returns you to ${escape(returnHost(summary))}`;

export const authorizePage = (options: {
  requestId: string;
  summary: RequestSummary;
  approvalPort: number;
}): string => {
  const shared = connection(options.summary);
  return render(
    "Connect",
    "connection request",
    patch(templates.authorize, {
      ...shared,
      approvalPort: String(options.approvalPort),
      requestId: escape(options.requestId),
      returnsTo: returnsTo(options.summary, "approving"),
    }),
  );
};

export interface SharedTool {
  name: string;
  dangerous: boolean;
}

const toolList = (tools: ReadonlyArray<SharedTool>): string => {
  if (tools.length === 0) return "";
  const ordered = [...tools].sort((left, right) => Number(right.dangerous) - Number(left.dangerous));
  const items = ordered
    .map((tool) => patch(tool.dangerous ? templates.dangerTool : templates.tool, { name: escape(tool.name) }))
    .join("");
  return patch(templates.toolList, { items });
};

export const approvalPage = (options: {
  requestId: string;
  summary: RequestSummary;
  tools: ReadonlyArray<SharedTool>;
  hostname: string;
  approvalPort: number;
  csrf: string;
}): string => {
  const shared = connection(options.summary);
  return render(
    "Approve",
    `porchlight on ${options.hostname}`,
    patch(templates.approval, {
      ...shared,
      tools: toolList(options.tools),
      approvalPort: String(options.approvalPort),
      requestId: escape(options.requestId),
      csrf: escape(options.csrf),
      returnsTo: returnsTo(options.summary, "allowing"),
    }),
  );
};
