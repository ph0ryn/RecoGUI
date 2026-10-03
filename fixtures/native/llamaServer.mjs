import { appendFileSync } from "node:fs";
import { createServer } from "node:http";

const args = process.argv.slice(2);
const argument = (name) => args[args.indexOf(name) + 1];
const scenario = process.env.RECOGUI_FIXTURE_SCENARIO;
const record = (value) =>
  appendFileSync(process.env.RECOGUI_FIXTURE_LOG, `${JSON.stringify(value)}\n`);

record({
  event: "start",
  model: argument("--model"),
  pid: process.pid,
  projector: argument("--mmproj"),
});

if (scenario === "earlyExit") {
  process.stderr.write("fixture model could not be loaded\n");
  process.exit(23);
}

let requests = 0;
const server = createServer(async (request, response) => {
  const send = (status, value) => {
    response.writeHead(status, { "Content-Type": "application/json" });
    response.end(JSON.stringify(value));
  };

  if (request.url === "/health") {
    if (scenario === "loading") {
      send(503, { status: "loading model" });
    } else {
      send(200, { status: "ok" });
    }

    return;
  }

  if (request.headers.authorization !== `Bearer ${argument("--api-key")}`) {
    send(401, { error: "authorization is required" });

    return;
  }

  if (request.url === "/props") {
    send(200, { media_marker: "<__media_fixture__>", modalities: { audio: true } });

    return;
  }

  if (request.url !== "/completion") {
    send(404, { error: "unknown endpoint" });

    return;
  }

  let body = "";

  for await (const chunk of request) {
    body += chunk;
  }

  const value = JSON.parse(body);

  record({ event: "completion", value });
  requests += 1;

  if (scenario === "http500") {
    send(500, { error: "fixture inference failed" });

    return;
  }

  if (scenario === "malformed") {
    send(200, { content: "missing required token counters" });

    return;
  }

  if (scenario === "blocked") {
    return;
  }

  let content = "language Japanese<asr_text>これはテストです。<|im_end|>";
  let stopType = "eos";

  if (scenario === "untagged") {
    content = "これはテストです。";
  }

  if (scenario === "limit" && requests === 1) {
    stopType = "limit";
  }

  send(200, {
    content,
    stop_type: stopType,
    tokens_evaluated: 100,
    tokens_predicted: 8,
    truncated: false,
  });
});

server.listen(Number(argument("--port")), argument("--host"));
