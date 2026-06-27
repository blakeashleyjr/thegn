import {
  ExtensionAPI,
  ExtensionContext,
  MessageUpdateEvent,
  AgentEndEvent,
  ToolExecutionStartEvent,
  ToolExecutionUpdateEvent,
  ToolExecutionEndEvent
} from "@earendil-works/pi-coding-agent";
import * as net from "net";
import { createRequire } from "module";

export default function (pi: ExtensionAPI) {
  let server: net.Server | null = null;
  let activeSocket: net.Socket | null = null;

  // We register a CLI flag to specify the ACP port
  pi.registerFlag("acp-port", {
    type: "string",
    description: "Port to start the ACP server on",
    default: "0" // 0 means pick an ephemeral port, but user can override
  });

  // Lower plane: route the model through superzej's proxy. pi flushes provider
  // registrations at startup (before session_start), so this MUST happen here in
  // the factory — not at runtime via providers/set. superzej hands us the proxy
  // config via env at spawn. The proxy speaks the Anthropic Messages API
  // (api "anthropic-messages"); baseUrl is the host root (pi appends /v1/messages).
  // Selected at session_start (setModel is invalid during the factory — the
  // runtime isn't up yet — but registerProvider IS flushed from here).
  // Full network seal (B2): under `network=none` the sealed agent has no IP
  // egress at all. SUPERZEJ_PROXY_UNIX points at a bind-mounted unix socket the
  // host relays to the model proxy; route every outbound connection through it
  // (the proxy is the agent's *only* egress, so a global dispatcher is exactly
  // right). The placeholder baseUrl host is ignored — undici dials the socket.
  const proxyUnix = process.env.SUPERZEJ_PROXY_UNIX;
  if (proxyUnix) {
    installUnixProxyFetch(proxyUnix);
  }

  let proxyModel: string | null = null;
  const proxyBase = process.env.SUPERZEJ_PROXY_BASE_URL;
  if (proxyBase) {
    const api = process.env.SUPERZEJ_PROXY_API || "anthropic-messages";
    proxyModel = process.env.SUPERZEJ_PROXY_MODEL || "model-proxy/standard";
    const apiKey = process.env.SUPERZEJ_PROXY_KEY || "sk-superzej";
    pi.registerProvider("superzej-proxy", {
      baseUrl: proxyBase,
      api,
      apiKey,
      models: [{
        id: proxyModel, name: proxyModel, reasoning: false, input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 200000, maxTokens: 8192
      }]
    } as any);
  }

  // State to track active ACP requests
  let currentPromptId: string | null = null;
  let nextRpcId = 1;
  const pendingRequests = new Map<number, { resolve: (val: any) => void, reject: (err: any) => void }>();

  // State to track encapsulated MCP requests
  let nextMcpId = 1;
  const pendingMcpRequests = new Map<number, { resolve: (val: any) => void, reject: (err: any) => void }>();

  // Send a JSON-RPC *request* to the host (superzej) and await its reply. Used by
  // the bouncer tool overrides: the host services the call (inside the sealed
  // sandbox, behind the approval gate) and replies with the result. Mirrors the
  // host->extension request path, in the other direction.
  function sendHostRequest(method: string, params?: any): Promise<any> {
    return new Promise((resolve, reject) => {
      if (!activeSocket) {
        return reject(new Error("No active ACP connection."));
      }
      const id = nextRpcId++;
      pendingRequests.set(id, { resolve, reject });
      sendAcp(activeSocket, { jsonrpc: "2.0", id, method, params });
    });
  }

  function sendMcpRequest(connectionId: string, method: string, params?: any): Promise<any> {
    return new Promise((resolve, reject) => {
      if (!activeSocket) {
        return reject(new Error("No active ACP connection."));
      }
      const mcpId = nextMcpId++;
      pendingMcpRequests.set(mcpId, { resolve, reject });
      
      // We send the encapsulated MCP message over ACP as a notification (per ACP spec for encapsulated streams)
      // or as an RPC call if the client expects to ack it. We'll use a notification.
      sendAcp(activeSocket, {
        jsonrpc: "2.0",
        method: "mcp/message",
        params: {
          connectionId,
          message: {
            jsonrpc: "2.0",
            id: mcpId,
            method,
            params
          }
        }
      });
    });
  }

  pi.on("session_start", async (event, ctx) => {
    // Only start the server once
    if (server) return;

    // superzej hands us the port it already bound. Prefer the --acp-port flag
    // when explicitly set, otherwise the ACP_PORT env var (the reliable path:
    // env crosses superzej's `sh -lc` + sandbox wrapping, an appended flag does
    // not). We MUST bind exactly this port — falling back to 0 (OS-ephemeral)
    // would make superzej connect to the wrong port.
    // Prefer a bind-mounted unix socket (sealed sandbox — crosses the netns
    // without network); otherwise the TCP port (non-sandboxed). superzej sets one.
    const socketPath = process.env.ACP_SOCKET;
    const flagStr = pi.getFlag("acp-port") as string;
    const portStr = flagStr && flagStr !== "0" ? flagStr : (process.env.ACP_PORT ?? "0");
    const port = parseInt(portStr, 10) || 0;
    if (!socketPath && port === 0) {
      ctx.ui.setStatus("acp", "ACP: no ACP_SOCKET/ACP_PORT provided — not starting server");
      return;
    }

    server = net.createServer((socket) => {
      activeSocket = socket;
      
      let buffer = "";
      socket.on("data", (data) => {
        buffer += data.toString();
        let newlineIndex;
        while ((newlineIndex = buffer.indexOf("\n")) !== -1) {
          const line = buffer.slice(0, newlineIndex);
          buffer = buffer.slice(newlineIndex + 1);
          if (line.trim()) {
            handleAcpMessage(line, socket, ctx);
          }
        }
      });

      socket.on("close", () => {
        if (activeSocket === socket) activeSocket = null;
      });
    });

    if (socketPath) {
      server.listen(socketPath, () => {
        ctx.ui.setStatus("acp", `ACP Server listening on ${socketPath}`);
      });
    } else {
      server.listen(port, "127.0.0.1", () => {
        const addr = server?.address();
        const actualPort = typeof addr === "object" ? addr?.port : port;
        ctx.ui.setStatus("acp", `ACP Server listening on port ${actualPort}`);
      });
    }

    // "The bouncer" (SUPERZEJ_BOUNCER=1): override pi's built-in file/shell tools
    // so they route back to superzej over ACP. superzej runs them inside the
    // sealed sandbox and gates the consequential ones (shell/edit/write) behind an
    // allow/deny overlay — so the sealed agent's "hands" are the host's, behind a
    // bouncer. Without this env (the additive default) pi keeps its own in-process
    // tools and edits auto-apply. See run.rs `dispatch_acp_inbound`.
    if (process.env.SUPERZEJ_BOUNCER === "1") {
      registerBouncerTools();
    }
  });

  // Register the bouncer tool overrides (idempotent). Each routes to the host
  // method `dispatch_acp_inbound` services; a denied gate comes back as a
  // JSON-RPC error, surfaced to pi as a failed tool call.
  function registerBouncerTools() {
    const text = (t: string, isError = false) => ({
      content: [{ type: "text", text: t }],
      isError
    });
    const fail = (e: any) => text(String(e?.message || e), true);

    pi.registerTool({
      name: "bash",
      label: "bash (superzej)",
      description: "Run a shell command inside the worktree's sealed sandbox (gated).",
      parameters: { type: "object", properties: { command: { type: "string" } }, required: ["command"] } as any,
      async execute(_id, params) {
        try {
          const r = await sendHostRequest("terminal/create", { command: params.command });
          return text(r?.output ?? "");
        } catch (e) { return fail(e); }
      }
    });
    pi.registerTool({
      name: "read",
      label: "read (superzej)",
      description: "Read a worktree file via superzej (scoped to the sandbox).",
      parameters: { type: "object", properties: { path: { type: "string" } }, required: ["path"] } as any,
      async execute(_id, params) {
        try {
          const r = await sendHostRequest("fs/read_text_file", { path: params.path });
          return text(r?.text ?? "");
        } catch (e) { return fail(e); }
      }
    });
    pi.registerTool({
      name: "edit",
      label: "edit (superzej)",
      description: "Edit a worktree file via superzej (gated allow/deny).",
      parameters: {
        type: "object",
        properties: {
          path: { type: "string" },
          edits: { type: "array", items: { type: "object" } }
        },
        required: ["path", "edits"]
      } as any,
      async execute(_id, params) {
        try {
          const r = await sendHostRequest("superzej/edit", { path: params.path, edits: params.edits });
          return text(`edit ${r?.status ?? "applied"}: ${params.path}`);
        } catch (e) { return fail(e); }
      }
    });
    pi.registerTool({
      name: "write",
      label: "write (superzej)",
      description: "Write a worktree file via superzej (gated allow/deny).",
      parameters: {
        type: "object",
        properties: { path: { type: "string" }, content: { type: "string" } },
        required: ["path", "content"]
      } as any,
      async execute(_id, params) {
        try {
          const r = await sendHostRequest("superzej/write", { path: params.path, content: params.content });
          return text(`write ${r?.status ?? "applied"}: ${params.path}`);
        } catch (e) { return fail(e); }
      }
    });

    // Make the overrides active alongside whatever pi already had.
    try {
      const active = pi.getActiveTools();
      pi.setActiveTools([...new Set([...active, "bash", "read", "edit", "write"])]);
    } catch { /* getActiveTools/setActiveTools optional across pi versions */ }
  }

  function sendAcp(socket: net.Socket, msg: any) {
    socket.write(JSON.stringify(msg) + "\n");
  }

  let activeTraceparent: string | undefined = undefined;

  function handleAcpMessage(line: string, socket: net.Socket, ctx: ExtensionContext) {
    let msg: any;
    try {
      msg = JSON.parse(line);
    } catch (e) {
      return;
    }

    if (msg.id !== undefined && pendingRequests.has(msg.id)) {
      // It's a response to an RPC request we sent to the client (superzej)
      const req = pendingRequests.get(msg.id)!;
      pendingRequests.delete(msg.id);
      if (msg.error) {
        req.reject(new Error(msg.error.message || "Unknown error"));
      } else {
        req.resolve(msg.result);
      }
      return;
    }

    if (msg.method === "initialize") {
      sendAcp(socket, {
        jsonrpc: "2.0",
        id: msg.id,
        result: {
          protocolVersion: "1.0",
          agentCapabilities: {
            mcpCapabilities: {
              acp: true
            }
          },
          agentInfo: {
            name: "pi-superzej-acp",
            version: "1.0.0"
          }
        }
      });
    } else if (msg.method === "session/prompt") {
      currentPromptId = msg.id;
      // Capture OTEL traceparent for context propagation
      if (msg.params._meta?.traceparent) {
        activeTraceparent = msg.params._meta.traceparent;
      }
      // Dispatch the user's prompt into pi. `sendUserMessage` lives on the
      // ExtensionAPI (`pi`), not on `ctx.ui` — it always triggers a turn.
      const promptText = msg.params.prompt;
      pi.sendUserMessage(promptText, { deliverAs: "steer" });
      
      // We don't send the result yet; we stream updates and send the result on agent_end
    } else if (msg.method === "session/close") {
      sendAcp(socket, { jsonrpc: "2.0", id: msg.id, result: {} });
    } else if (msg.method === "session/set_config_option") {
      const { optionId, value } = msg.params;
      if (optionId === "thinking_level") {
        pi.setThinkingLevel(value);
      }
      if (msg.id) sendAcp(socket, { jsonrpc: "2.0", id: msg.id, result: {} });
    } else if (msg.method === "providers/set") {
      // The host (superzej) is bridging the U layer (szproxy) over ACP!
      const { id: providerId, baseUrl, headers, apiType, models } = msg.params;
      
      const apiKey = headers?.Authorization?.replace("Bearer ", "") || "szk-virtual";
      
      pi.registerProvider(providerId, {
        baseUrl,
        api: apiType || "openai",
        apiKey,
        models: models || [
          {
            id: "superzej-default",
            name: "Superzej Routed Model",
            reasoning: false,
            input: ["text", "image"],
            cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
            contextWindow: 200000,
            maxTokens: 16384
          }
        ]
      });

      // We attempt to actively switch to the newly registered provider model
      pi.setModel({ id: models?.[0]?.id || "superzej-default", provider: providerId } as any);
      
      if (msg.id) sendAcp(socket, { jsonrpc: "2.0", id: msg.id, result: {} });
    } else if (msg.method === "mcp/connect") {
      // superzej is providing an MCP connection
      const connectionId = msg.params.connectionId;
      if (msg.id) sendAcp(socket, { jsonrpc: "2.0", id: msg.id, result: {} });
      
      // Bootstrap the connection by asking for tools
      discoverMcpTools(connectionId);
    } else if (msg.method === "mcp/message") {
      const inner = msg.params.message;
      if (inner.id !== undefined && pendingMcpRequests.has(inner.id)) {
        const req = pendingMcpRequests.get(inner.id)!;
        pendingMcpRequests.delete(inner.id);
        if (inner.error) req.reject(new Error(inner.error.message || "MCP Error"));
        else req.resolve(inner.result);
      }
    }
  }

  async function discoverMcpTools(connectionId: string) {
    try {
      const result = await sendMcpRequest(connectionId, "tools/list");
      if (result && result.tools) {
        for (const tool of result.tools) {
          // Dynamic tool registration
          pi.registerTool({
            name: tool.name,
            label: tool.name, // Human readable
            description: tool.description || `MCP tool from ${connectionId}`,
            // We pass the raw JSON schema. At runtime, TypeBox TSchema is just a JSON schema object.
            parameters: tool.inputSchema as any,
            async execute(toolCallId, params, signal, onUpdate, ctx) {
              const callResult = await sendMcpRequest(connectionId, "tools/call", {
                name: tool.name,
                arguments: params
              });

              // Format the MCP result back to pi's ToolResult event structure
              let textContent = "";
              if (callResult.content && Array.isArray(callResult.content)) {
                textContent = callResult.content
                  .filter((c: any) => c.type === "text")
                  .map((c: any) => c.text)
                  .join("\n");
              }
              
              return {
                content: [{ type: "text", text: textContent || "Tool executed successfully." }],
                details: callResult,
                isError: callResult.isError || false
              };
            }
          });
        }
        // Inform pi that tools have changed
        const activeTools = pi.getActiveTools();
        const newToolNames = result.tools.map((t: any) => t.name);
        pi.setActiveTools([...activeTools, ...newToolNames]);
      }
    } catch (e) {
      console.error(`Failed to discover MCP tools for ${connectionId}:`, e);
    }
  }

  // --- Map Pi Events to ACP Streaming Updates ---

  pi.on("message_update", (event: MessageUpdateEvent, ctx: ExtensionContext) => {
    if (!activeSocket) return;

    if (event.assistantMessageEvent.type === "text") {
      sendAcp(activeSocket, {
        jsonrpc: "2.0",
        method: "session/update",
        params: {
          id: currentPromptId,
          type: "agent_message_chunk",
          content: event.assistantMessageEvent.text
        }
      });
    } else if (event.assistantMessageEvent.type === "reasoning") {
      sendAcp(activeSocket, {
        jsonrpc: "2.0",
        method: "session/update",
        params: {
          id: currentPromptId,
          type: "agent_thought_chunk",
          content: event.assistantMessageEvent.text
        }
      });
    }
  });

  pi.on("tool_execution_start", (event: ToolExecutionStartEvent) => {
    if (!activeSocket) return;
    
    sendAcp(activeSocket, {
      jsonrpc: "2.0",
      method: "session/update",
      params: {
        id: currentPromptId,
        type: "tool_call",
        toolCallId: event.toolCallId,
        toolName: event.toolName,
        args: event.args
      }
    });
  });

  pi.on("tool_execution_end", (event: ToolExecutionEndEvent) => {
    if (!activeSocket) return;
    
    sendAcp(activeSocket, {
      jsonrpc: "2.0",
      method: "session/update",
      params: {
        id: currentPromptId,
        type: "tool_call_update",
        toolCallId: event.toolCallId,
        status: event.isError ? "failed" : "completed",
        result: event.result
      }
    });
  });

  pi.on("turn_end", (event, ctx) => {
    if (!activeSocket) return;

    // Report context usage back to superzej for live tracking
    const usage = ctx.getContextUsage();
    if (usage && usage.tokens !== null) {
      sendAcp(activeSocket, {
        jsonrpc: "2.0",
        method: "session/update",
        params: {
          id: currentPromptId,
          type: "usage_update",
          used: usage.tokens,
          size: usage.contextWindow
        }
      });
    }
  });

  pi.on("thinking_level_select", (event) => {
    if (!activeSocket) return;
    sendAcp(activeSocket, {
      jsonrpc: "2.0",
      method: "session/update",
      params: {
        id: currentPromptId,
        type: "config_option_update",
        optionId: "thinking_level",
        value: event.level
      }
    });
  });

  pi.on("agent_end", (event: AgentEndEvent, ctx: ExtensionContext) => {
    if (!activeSocket) return;

    // Lifecycle: notify superzej the agent finished (fires for user-driven turns
    // too) → AgentDone/AgentFailed notification + clears the chip's running state.
    sendAcp(activeSocket, {
      jsonrpc: "2.0",
      method: "session/update",
      params: { id: currentPromptId, type: "agent_end", success: true }
    });

    // If superzej drove this turn via session/prompt, send its final response.
    if (currentPromptId) {
      sendAcp(activeSocket, {
        jsonrpc: "2.0",
        id: currentPromptId,
        result: { stopReason: "end_turn" }
      });
      currentPromptId = null;
    }
  });

  pi.on("session_shutdown", () => {
    if (server) {
      server.close();
      server = null;
    }
  });

  // B2 full network seal: route the model proxy over a bind-mounted unix socket.
  // Under `network=none` the sealed agent has loopback only, so the host relay is
  // reachable solely as a unix socket. `undici` (the global-fetch impl) isn't a
  // resolvable module in pi's runtime, but core `node:http` supports `socketPath`,
  // so we wrap `globalThis.fetch`: requests to the placeholder proxy host go
  // through the socket; everything else (there is nothing else, under the seal)
  // falls through. SSE streaming is preserved via `Readable.toWeb`.
  function installUnixProxyFetch(socketPath: string) {
    try {
      const req = createRequire(import.meta.url);
      const http = req("node:http");
      const { Readable } = req("node:stream");
      const PLACEHOLDER = "proxy.superzej.internal";
      const orig = globalThis.fetch;
      globalThis.fetch = ((input: any, init: any = {}) => {
        const raw = typeof input === "string" ? input : (input?.url ?? String(input));
        let url: URL;
        try { url = new URL(raw); } catch { return orig(input, init); }
        if (url.hostname !== PLACEHOLDER) return orig(input, init);

        // Normalize headers (Headers instance | plain object | array) → object.
        const headers: Record<string, string> = {};
        const h = init.headers ?? (typeof input === "object" ? input.headers : undefined);
        if (h) {
          if (typeof h.forEach === "function" && !Array.isArray(h)) h.forEach((v: string, k: string) => (headers[k] = v));
          else if (Array.isArray(h)) for (const [k, v] of h) headers[k] = v as string;
          else for (const k of Object.keys(h)) headers[k] = h[k];
        }
        const method = (init.method || (typeof input === "object" ? input.method : "GET") || "GET").toUpperCase();
        const body = init.body ?? (typeof input === "object" ? input.body : undefined);

        return new Promise((resolve, reject) => {
          const r = http.request(
            { socketPath, path: url.pathname + url.search, method, headers },
            (res: any) => {
              resolve(new Response(Readable.toWeb(res) as any, {
                status: res.statusCode || 502,
                statusText: res.statusMessage || "",
                headers: res.headers as any,
              }));
            }
          );
          r.on("error", reject);
          if (body == null) r.end();
          else if (typeof body === "string" || body instanceof Uint8Array || Buffer.isBuffer(body)) r.end(body);
          else if (typeof body.pipe === "function") body.pipe(r);
          else r.end(String(body));
        });
      }) as any;
    } catch (e) {
      console.error("superzej: failed to route the model proxy over the unix socket:", e);
    }
  }

  // Tool ownership is mode-dependent:
  //  - Additive (default): superzej does NOT override pi's built-in bash/read/
  //    edit/write. pi runs them natively (in-process, sandboxed iff its pane is),
  //    keeping pi's inline-diff edit UX; superzej stays additive (observe via
  //    session/update, route the model via the proxy, house tools over MCP).
  //  - Bouncer (SUPERZEJ_BOUNCER=1, see `registerBouncerTools`): those four tools
  //    are overridden to route over ACP so superzej runs them inside the sealed
  //    sandbox and gates shell/edit/write behind an allow/deny overlay.
}
