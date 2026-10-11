import { http } from './client';

/**
 * Live canvas API — the spatial brain map backed by the gateway.
 *
 * Unlike a sketching tool, canvas nodes can mirror live records: a node with
 * `kind: 'task'` carries a `live` block with the task's CURRENT title and
 * status, joined server-side on every read. Requests without an `entityId`
 * land on the viewer's private Personal Space canvas.
 */

export type CanvasNodeKind =
  | 'text'
  | 'note'
  | 'link'
  | 'shape'
  | 'task'
  | 'mission'
  | 'entity';

export interface CanvasLiveRef {
  title: string;
  status: string;
  exists: boolean;
}

export interface CanvasNode {
  id: string;
  workspaceId: string;
  entityId: string;
  kind: CanvasNodeKind;
  refId?: string;
  label?: string;
  content?: string;
  x: number;
  y: number;
  w?: number;
  h?: number;
  color?: string;
  createdBy: string;
  createdAt: string;
  updatedAt: string;
  /** Present on live kinds (task/mission/entity): the record's current state. */
  live?: CanvasLiveRef;
}

export interface CanvasEdge {
  id: string;
  workspaceId: string;
  entityId: string;
  fromNode: string;
  toNode: string;
  label?: string;
  createdBy: string;
  createdAt: string;
}

export interface CanvasData {
  entityId: string;
  nodes: CanvasNode[];
  edges: CanvasEdge[];
}

export interface CreateNodeInput {
  entityId?: string;
  kind: CanvasNodeKind;
  refId?: string;
  label?: string;
  content?: string;
  x: number;
  y: number;
  w?: number;
  h?: number;
  color?: string;
}

export interface UpdateNodeInput {
  label?: string;
  content?: string;
  x?: number;
  y?: number;
  w?: number;
  h?: number;
  color?: string;
}

export async function getCanvas(entityId?: string): Promise<CanvasData> {
  const q = entityId ? `?entity_id=${encodeURIComponent(entityId)}` : '';
  return http.get<CanvasData>(`/api/canvas${q}`);
}

export async function createCanvasNode(input: CreateNodeInput): Promise<CanvasNode> {
  return http.post<CanvasNode>('/api/canvas/nodes', input);
}

export async function updateCanvasNode(
  id: string,
  input: UpdateNodeInput,
): Promise<CanvasNode> {
  return http.patch<CanvasNode>(`/api/canvas/nodes/${encodeURIComponent(id)}`, input);
}

export async function deleteCanvasNode(id: string): Promise<void> {
  await http.del(`/api/canvas/nodes/${encodeURIComponent(id)}`);
}

export async function createCanvasEdge(
  fromNode: string,
  toNode: string,
  label?: string,
): Promise<CanvasEdge> {
  return http.post<CanvasEdge>('/api/canvas/edges', { fromNode, toNode, label });
}

export async function deleteCanvasEdge(id: string): Promise<void> {
  await http.del(`/api/canvas/edges/${encodeURIComponent(id)}`);
}
