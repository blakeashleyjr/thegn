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
    const flagStr = pi.getFlag("acp-port") as string;
    const portStr = flagStr && flagStr !== "0" ? flagStr : (process.env.ACP_PORT ?? "0");
    const port = parseInt(portStr, 10) || 0;
    if (port === 0) {
      ctx.ui.setStatus("acp", "ACP: no port provided (ACP_PORT unset) — not starting server");
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

    server.listen(port, "127.0.0.1", () => {
      const addr = server?.address();
      const actualPort = typeof addr === "object" ? addr?.port : port;
      // Tell superzej where to connect
      ctx.ui.setStatus("acp", `ACP Server listening on port ${actualPort}`);
    });
  });

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

  // NOTE: superzej deliberately does NOT override pi's built-in bash/read/edit/
  // write tools. pi runs them natively (in-process, sandboxed iff its pane is),
  // which keeps pi's native inline-diff edit UX and avoids a dead host round-trip.
  // superzej stays additive — observing via session/update, routing the model via
  // the proxy, and exposing house tools via MCP-over-ACP. The real "bouncer"
  // (sealed sandbox + a unix-socket ACP transport + a review gate) is a separate,
  // deferred effort; the host-side terminal/create + fs servicing remain in
  // run.rs for that future, currently unexercised.
}
