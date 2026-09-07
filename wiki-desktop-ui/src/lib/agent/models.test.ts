/**
 * @vitest-environment jsdom
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

vi.mock("@/api/server", () => ({
  loadSyncConfig: vi.fn(),
}));

vi.mock("@/lib/server-auth", () => ({
  getToken: vi.fn(),
}));

vi.mock("@/lib/ai-client", () => ({
  listRelayModels: vi.fn(),
}));

vi.mock("./client", () => ({
  modelList: vi.fn(),
}));

import { loadSyncConfig } from "@/api/server";
import { getToken } from "@/lib/server-auth";
import { listRelayModels } from "@/lib/ai-client";
import { modelList } from "./client";
import { listAvailableModels } from "./models";

const mockedLoadSyncConfig = vi.mocked(loadSyncConfig);
const mockedGetToken = vi.mocked(getToken);
const mockedListRelayModels = vi.mocked(listRelayModels);
const mockedModelList = vi.mocked(modelList);

describe("listAvailableModels", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("gateway 模式正常返回 relay 模型映射（无 efforts 元数据）", async () => {
    mockedLoadSyncConfig.mockReturnValue({
      baseUrl: "https://example.com",
      knowledgeId: "k1",
      propagateDeletes: false,
      autoSyncAfterSave: true,
      syncIntervalMinutes: 0,
    });
    mockedGetToken.mockReturnValue("tok");
    mockedListRelayModels.mockResolvedValue(["gpt-4o", "o1-mini"]);

    const res = await listAvailableModels("gateway");

    expect(mockedListRelayModels).toHaveBeenCalledWith("https://example.com");
    expect(res).toEqual([
      { id: "gpt-4o", label: "gpt-4o", efforts: [], defaultEffort: null },
      { id: "o1-mini", label: "o1-mini", efforts: [], defaultEffort: null },
    ]);
    expect(mockedModelList).not.toHaveBeenCalled();
  });

  it("gateway 模式无 baseUrl 抛错文案", async () => {
    mockedLoadSyncConfig.mockReturnValue(null);

    await expect(listAvailableModels("gateway")).rejects.toThrow("未连接服务器，请在设置中完成登录");
    expect(mockedListRelayModels).not.toHaveBeenCalled();
    expect(mockedModelList).not.toHaveBeenCalled();
  });

  it("gateway 模式 baseUrl 为空串抛错", async () => {
    mockedLoadSyncConfig.mockReturnValue({
      baseUrl: "   ",
      knowledgeId: "k1",
      propagateDeletes: false,
    });

    await expect(listAvailableModels("gateway")).rejects.toThrow("未连接服务器，请在设置中完成登录");
  });

  it("gateway 模式无 token 抛错文案", async () => {
    mockedLoadSyncConfig.mockReturnValue({
      baseUrl: "https://example.com",
      knowledgeId: "k1",
      propagateDeletes: false,
    });
    mockedGetToken.mockReturnValue(null);

    await expect(listAvailableModels("gateway")).rejects.toThrow("未连接服务器，请在设置中完成登录");
    expect(mockedListRelayModels).not.toHaveBeenCalled();
  });

  it("openai-key 模式走 codex modelList 并正确映射 displayName", async () => {
    mockedModelList.mockResolvedValue({
      data: [
        { id: "m1", model: "m1-model", displayName: "M1", hidden: false } as unknown as Record<string, unknown>,
        { id: "m2", model: "", displayName: "", hidden: false } as unknown as Record<string, unknown>,
        { id: "m3", model: "m3", displayName: "", hidden: false } as unknown as Record<string, unknown>,
      ],
      nextCursor: null,
    } as unknown as Awaited<ReturnType<typeof modelList>>);

    const res = await listAvailableModels("openai-key");

    expect(mockedModelList).toHaveBeenCalledWith({});
    expect(res).toEqual([
      { id: "m1-model", label: "M1 (m1-model)", efforts: [], defaultEffort: null },
      { id: "m2", label: "m2", efforts: [], defaultEffort: null },
      { id: "m3", label: "m3", efforts: [], defaultEffort: null },
    ]);
    expect(mockedListRelayModels).not.toHaveBeenCalled();
  });

  it("chatgpt 模式同样走 codex modelList", async () => {
    mockedModelList.mockResolvedValue({
      data: [{ id: "gpt-4o", model: "gpt-4o", displayName: "GPT 4o", hidden: false } as unknown as Record<string, unknown>],
      nextCursor: null,
    } as unknown as Awaited<ReturnType<typeof modelList>>);

    const res = await listAvailableModels("chatgpt");

    expect(mockedModelList).toHaveBeenCalledWith({});
    expect(res).toEqual([{ id: "gpt-4o", label: "GPT 4o (gpt-4o)", efforts: [], defaultEffort: null }]);
  });

  it("gateway relay 返回空数组时返回空列表", async () => {
    mockedLoadSyncConfig.mockReturnValue({
      baseUrl: "https://example.com",
      knowledgeId: "k1",
      propagateDeletes: false,
    });
    mockedGetToken.mockReturnValue("tok");
    mockedListRelayModels.mockResolvedValue([]);

    const res = await listAvailableModels("gateway");
    expect(res).toEqual([]);
  });

  it("非 gateway：supportedReasoningEfforts/defaultReasoningEffort 映射到 efforts", async () => {
    mockedModelList.mockResolvedValue({
      data: [
        {
          id: "g1",
          model: "gpt-5",
          displayName: "GPT 5",
          hidden: false,
          supportedReasoningEfforts: [
            { reasoningEffort: "low", description: "快速、低开销" },
            { reasoningEffort: "medium", description: "" },
            { reasoningEffort: "high", description: "深度推理" },
          ],
          defaultReasoningEffort: "medium",
        } as unknown as Record<string, unknown>,
      ],
      nextCursor: null,
    } as unknown as Awaited<ReturnType<typeof modelList>>);

    const res = await listAvailableModels("openai-key");

    expect(res).toEqual([
      {
        id: "gpt-5",
        label: "GPT 5 (gpt-5)",
        efforts: [
          { effort: "low", description: "快速、低开销" },
          // 空 description 被丢弃，只保留 effort
          { effort: "medium" },
          { effort: "high", description: "深度推理" },
        ],
        defaultEffort: "medium",
      },
    ]);
  });

  it("非 gateway：缺失元数据字段时 efforts 为空、defaultEffort 为 null", async () => {
    mockedModelList.mockResolvedValue({
      data: [{ id: "m", model: "m", displayName: "", hidden: false } as unknown as Record<string, unknown>],
      nextCursor: null,
    } as unknown as Awaited<ReturnType<typeof modelList>>);

    const res = await listAvailableModels("openai-key");

    expect(res).toEqual([{ id: "m", label: "m", efforts: [], defaultEffort: null }]);
  });

  it("非 gateway：畸形 efforts 条目被过滤（缺 reasoningEffort / 非字符串）", async () => {
    mockedModelList.mockResolvedValue({
      data: [
        {
          id: "m",
          model: "m",
          displayName: "",
          hidden: false,
          supportedReasoningEfforts: [
            { reasoningEffort: "", description: "空" },
            { description: "无 effort 键" },
            { reasoningEffort: 42, description: "非字符串" },
            null,
            { reasoningEffort: "high", description: "深度" },
          ],
          defaultReasoningEffort: 42,
        } as unknown as Record<string, unknown>,
      ],
      nextCursor: null,
    } as unknown as Awaited<ReturnType<typeof modelList>>);

    const res = await listAvailableModels("openai-key");

    expect(res).toEqual([
      {
        id: "m",
        label: "m",
        efforts: [{ effort: "high", description: "深度" }],
        defaultEffort: null,
      },
    ]);
  });
});
