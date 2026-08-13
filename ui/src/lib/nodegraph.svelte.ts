// Node-graph editor state (M6 Phase 2). Nodes + edges compile to WGSL (server
// side) and feed the SAME Effect Studio preview/save pipeline.

import { nodegraphPalette, nodegraphCompile, type NodeKindDef } from '$lib/ipc';
import { studio, preview } from '$lib/studio.svelte';

export interface UINode {
  id: string;
  kind: string;
  x: number;
  y: number;
  params: Record<string, number>;
}
export interface UIEdge {
  from: string;
  to: string;
  to_input: string;
}

interface GraphState {
  nodes: UINode[];
  edges: UIEdge[];
  palette: NodeKindDef[];
  /** A pending connection started from this node's output. */
  pendingFrom: string | null;
  seq: number;
}

export const graph = $state<GraphState>({
  nodes: [],
  edges: [],
  palette: [],
  pendingFrom: null,
  seq: 0
});

let compileTimer: ReturnType<typeof setTimeout> | null = null;

export function kindDef(kind: string): NodeKindDef | undefined {
  return graph.palette.find((k) => k.key === kind);
}

export async function loadPalette(): Promise<void> {
  if (graph.palette.length) return;
  try {
    const r = await nodegraphPalette();
    graph.palette = r.kinds;
  } catch {
    /* leave empty */
  }
}

/** Seed a minimal Source → Brightness → Output graph the first time. */
export async function ensureSeeded(): Promise<void> {
  await loadPalette();
  if (graph.nodes.length) return;
  addNode('input', 40, 120);
  addNode('brightness', 250, 120);
  addNode('output', 470, 120);
  // Connect them.
  graph.edges = [
    { from: graph.nodes[0].id, to: graph.nodes[1].id, to_input: 'in' },
    { from: graph.nodes[1].id, to: graph.nodes[2].id, to_input: 'in' }
  ];
  compileSoon();
}

export function addNode(kind: string, x = 60, y = 60): void {
  const def = kindDef(kind);
  const params: Record<string, number> = {};
  def?.params.forEach((p) => (params[p.key] = p.default));
  graph.nodes = [...graph.nodes, { id: `n${graph.seq++}`, kind, x, y, params }];
  compileSoon();
}

export function moveNode(id: string, x: number, y: number): void {
  const n = graph.nodes.find((m) => m.id === id);
  if (n) {
    n.x = x;
    n.y = y;
  }
}

export function deleteNode(id: string): void {
  graph.nodes = graph.nodes.filter((n) => n.id !== id);
  graph.edges = graph.edges.filter((e) => e.from !== id && e.to !== id);
  compileSoon();
}

export function startConnect(nodeId: string): void {
  graph.pendingFrom = graph.pendingFrom === nodeId ? null : nodeId;
}

export function finishConnect(nodeId: string, input: string): void {
  if (!graph.pendingFrom || graph.pendingFrom === nodeId) {
    graph.pendingFrom = null;
    return;
  }
  // One edge per (target, input): replace any existing.
  graph.edges = graph.edges.filter((e) => !(e.to === nodeId && e.to_input === input));
  graph.edges = [...graph.edges, { from: graph.pendingFrom, to: nodeId, to_input: input }];
  graph.pendingFrom = null;
  compileSoon();
}

export function deleteEdge(i: number): void {
  graph.edges = graph.edges.filter((_, idx) => idx !== i);
  compileSoon();
}

export function setParam(id: string, key: string, value: number): void {
  const n = graph.nodes.find((m) => m.id === id);
  if (n) n.params[key] = value;
  compileSoon();
}

function compileSoon(): void {
  if (compileTimer) clearTimeout(compileTimer);
  compileTimer = setTimeout(() => void compileToStudio(), 250);
}

/** Compile the graph → set the Studio's body + params → live preview. */
export async function compileToStudio(): Promise<void> {
  try {
    const g = {
      nodes: graph.nodes.map((n) => ({
        id: n.id,
        kind: n.kind,
        params: Object.entries(n.params).map(([key, value]) => ({ key, value }))
      })),
      edges: graph.edges
    };
    const { body, params } = await nodegraphCompile(g);
    studio.body = body;
    studio.params = params.map((p) => ({ ...p, value: p.default }));
    studio.error = null;
    await preview();
  } catch (e) {
    studio.error = String(e);
  }
}
