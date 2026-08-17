// dsh-control: dsh (DeepSeek Harness) 控制面插件。
//
// 挂载方式（零侵入，不动 ~/.dsh/profiles/web）：
//   dsh --profile web --patch <本目录>/cordis.patch.yml
//
// 暴露端点（仅绑定 127.0.0.1）：
//   GET  /status    -> { ok, pid, node, uptimeMs, webPort, webUrl, profile }
//   POST /shutdown  -> 优雅退出整个 dsh 进程（走 dsh 的 appExit 服务）
//   POST /reload    -> 整树热重载（include.refresh()，与 dsh 的 hmr 同路径）

import { createServer } from "node:http";

export const name = "dsh-control";

const PORT = 3399;
const HOST = "127.0.0.1";
const log = (level, msg) => console[level]?.("[dsh-control] " + msg);

export function apply(ctx) {
  const startedAt = Date.now();

  const server = createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://x");
    const json = (code, body) => {
      res.writeHead(code, { "content-type": "application/json; charset=utf-8" });
      res.end(JSON.stringify(body));
    };

    if (req.method === "GET" && url.pathname === "/status") {
      const webPort = ctx.get("webServer")?.port;
      json(200, {
        ok: true,
        service: "dsh-control",
        pid: process.pid,
        node: process.version,
        uptimeMs: Date.now() - startedAt,
        webPort: webPort ?? null,
        webUrl: webPort ? `http://127.0.0.1:${webPort}` : null,
        profile: "web",
      });
      return;
    }

    if (req.method === "POST" && url.pathname === "/shutdown") {
      json(200, { ok: true, message: "shutting down dsh" });
      // 走 dsh 的 appExit 服务 → profile-boot 的 shutdown → fiber.dispose + 进程退出
      setImmediate(() => {
        const exit = ctx.get("appExit");
        if (typeof exit === "function") exit(0);
        else process.exit(0);
      });
      return;
    }

    if (req.method === "POST" && url.pathname === "/reload") {
      // 先响应（整树重载会 dispose 并重建本插件自身，HTTP 连接可能在重载中断开），
      // 再异步触发 include.refresh()（dsh 的整树热重载入口，与 hmr 改配置同路径）。
      json(200, { ok: true, message: "hot reload accepted, reloading tree" });
      setImmediate(async () => {
        try {
          const loader = ctx.get("loader");
          if (!loader) throw new Error("loader service unavailable");
          let reloaded = false;
          for (const entry of loader.entries()) {
            const include = entry.subtree;
            if (include?.refresh) {
              await include.refresh();
              reloaded = true;
              break;
            }
          }
          if (reloaded) log("info", "hot reload completed");
          else log("warn", "hot reload skipped — no include entry found");
        } catch (err) {
          log("warn", "hot reload failed: " + (err?.message ?? err));
        }
      });
      return;
    }

    json(404, { ok: false, message: "not found" });
  });

  server.on("error", (err) => log("warn", "server error: " + err.message));
  server.listen(PORT, HOST, () => {
    log("info", "listening on http://" + HOST + ":" + PORT);
  });

  // Cordis 清理回调统一走 ctx.effect（返回 disposer）
  ctx.effect(() => () => {
    server.close();
  }, "dsh-control: http server");
}
