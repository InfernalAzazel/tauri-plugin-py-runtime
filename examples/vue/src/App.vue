<script setup lang="ts">
import { ref } from "vue";
import { invoke } from "@tauri-apps/api/core";

interface PingResponse {
  value?: string;
}

const input = ref("Hello from Vue");
const response = ref("");
const error = ref("");

async function ping() {
  error.value = "";
  try {
    const res = await invoke<PingResponse>("plugin:py-runtime|ping", {
      payload: { value: input.value },
    });
    response.value = res.value ?? "";
  } catch (e) {
    error.value = String(e);
  }
}
</script>

<template>
  <main class="container">
    <h1>Tauri Plugin py-runtime</h1>
    <form class="row" @submit.prevent="ping">
      <input v-model="input" placeholder="Enter a value..." />
      <button type="submit">Ping</button>
    </form>
    <p v-if="response">Response: {{ response }}</p>
    <p v-if="error" class="error">{{ error }}</p>
  </main>
</template>

<style scoped>
.container {
  font-family: system-ui, sans-serif;
  max-width: 480px;
  margin: 48px auto;
  text-align: center;
}

.row {
  display: flex;
  gap: 8px;
  justify-content: center;
}

input {
  flex: 1;
  padding: 6px 10px;
}

.error {
  color: #d33;
}
</style>
