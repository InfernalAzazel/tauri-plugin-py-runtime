<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import {
  Channel,
  PyRuntimeError,
  pyEmit,
  pyInvoke,
  pyListen,
  pyStatus,
  type PyStatus,
  type UnlistenFn,
} from "tauri-plugin-py-runtime-api";

const status = ref<PyStatus | "unknown">("unknown");
const name = ref("Kylin");
const progress = ref(0);
const logs = ref<string[]>([]);
const unlisteners: UnlistenFn[] = [];

function record(line: string) {
  logs.value.unshift(`${new Date().toLocaleTimeString()}  ${line}`);
}

async function run(label: string, task: () => Promise<unknown>) {
  try {
    record(`${label} => ${JSON.stringify(await task())}`);
  } catch (e) {
    const err = e instanceof PyRuntimeError ? e : new PyRuntimeError("internal", String(e));
    record(`${label} failed [${err.code}] ${err.message}`);
  }
}

const hello = () => run("hello", () => pyInvoke<string>("hello", { name: name.value }));

const raw = () =>
  run("reverse (raw)", async () => {
    const bytes = await pyInvoke<Uint8Array>("reverse", new TextEncoder().encode("abc123"));
    return new TextDecoder().decode(bytes);
  });

const concurrent = () =>
  run("concurrent", async () => {
    const order: number[] = [];
    await Promise.all(
      [800, 100].map((ms) => pyInvoke<number>("sleep", { ms }).then((v) => order.push(v))),
    );
    return { finishedOrder: order };
  });

const fail = () => run("fail", () => pyInvoke("fail", { message: "something went wrong" }));

const timeout = () => run("timeout", () => pyInvoke("sleep", { ms: 2000 }, { timeoutMs: 500 }));

const cancel = () =>
  run("cancel", () => {
    const controller = new AbortController();
    setTimeout(() => controller.abort(), 300);
    return pyInvoke("sleep", { ms: 3000 }, { signal: controller.signal });
  });

const stream = () =>
  run("progress (channel)", () => {
    progress.value = 0;
    const onProgress = new Channel<number>();
    onProgress.onmessage = (step) => (progress.value = step);
    return pyInvoke<string>("progress", { steps: 5, onProgress });
  });

const ping = () => run("emit ping", () => pyEmit("ping", { at: Date.now() }));

onMounted(async () => {
  unlisteners.push(await pyListen("pong", (e) => record(`pong <= ${JSON.stringify(e.payload)}`)));
  unlisteners.push(await pyListen("exited", (e) => record(`python exited ${JSON.stringify(e.payload)}`)));
  status.value = await pyStatus();
});

onUnmounted(() => unlisteners.forEach((fn) => fn()));
</script>

<template>
  <main class="container">
    <h1>Tauri Plugin py-runtime</h1>
    <p>Runtime status: <strong>{{ status }}</strong></p>

    <section class="row">
      <input v-model="name" placeholder="Your name" />
      <button @click="hello">Hello</button>
    </section>

    <section class="grid">
      <button @click="raw">Raw bytes</button>
      <button @click="concurrent">Concurrent</button>
      <button @click="fail">Error</button>
      <button @click="timeout">Timeout</button>
      <button @click="cancel">Cancel</button>
      <button @click="stream">Channel</button>
      <button @click="ping">Event</button>
    </section>

    <progress :value="progress" max="5" />

    <ul class="log">
      <li v-for="(line, i) in logs" :key="i">{{ line }}</li>
    </ul>
  </main>
</template>

<style scoped>
.container {
  font-family: system-ui, sans-serif;
  max-width: 640px;
  margin: 32px auto;
}

.row {
  display: flex;
  gap: 8px;
}

.row input {
  flex: 1;
  padding: 6px 10px;
}

.grid {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 8px;
  margin: 16px 0;
}

progress {
  width: 100%;
}

.log {
  font-family: ui-monospace, monospace;
  font-size: 12px;
  list-style: none;
  padding: 0;
  max-height: 280px;
  overflow: auto;
}
</style>
