// dsh-tray-control: dsh (DeepSeek Harness) 控制面插件。
//
// 挂载方式（零侵入，不动 ~/.dsh/profiles/web）：
//   dsh --profile web --patch <本目录>/cordis.patch.yml
//
// 暴露端点（仅绑定 127.0.0.1）：
//   GET  /status    -> { ok, pid, node, uptimeMs, webPort, webUrl, profile }
//   POST /shutdown  -> 优雅退出整个 dsh 进程（走 dsh 的 appExit 服务）
//   POST /reload    -> 热重载（v1 占位，后续实现 loader 整树重载）

import { createServer } from "node:http";

export const name = "dsh-tray-control";

export const inject = ["webServer"];

const PORT = 3399;
const HOST = "127.0.0.1";

export function apply(ctx) {
  const startedAt = Date.now();
  const logger = ctx.logger ?? console;

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
        service: "dsh-tray-control",
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
      json(501, { ok: false, message: "hot reload not implemented yet (v1 placeholder)" });
      return;
    }

    json(404, { ok: false, message: "not found" });
  });

  server.on("error", (err) => logger.warn("dsh-tray-control server error: %s", err.message));
  server.listen(PORT, HOST, () => {
    logger.info("dsh-tray-control listening on http://%s:%d", HOST, PORT);
  });

  ctx.onDispose(() => {
    server.close();
  });
}
