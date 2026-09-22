package llmshim

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
)

func TestChat(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/v1/chat" {
			t.Errorf("unexpected request: %s %s", r.Method, r.URL.Path)
		}
		var req ChatRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			t.Fatalf("decode request: %v", err)
		}
		if req.Model != "anthropic/claude-sonnet-4-6" {
			t.Errorf("model = %q", req.Model)
		}
		if len(req.Messages) != 1 || req.Messages[0].Content != "Hello!" {
			t.Errorf("messages = %+v", req.Messages)
		}
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{
			"id": "resp_123",
			"model": "claude-sonnet-4-6",
			"provider": "anthropic",
			"message": {"role": "assistant", "content": "Hi there!"},
			"reasoning": "thinking...",
			"usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15, "cost_usd": 1.0, "cost_source": "provider_floor"},
			"latency_ms": 420
		}`)
	}))
	defer srv.Close()

	c := New(WithBaseURL(srv.URL))
	resp, err := c.Chat(context.Background(), ChatRequest{
		Model:    "anthropic/claude-sonnet-4-6",
		Messages: []Message{{Role: "user", Content: "Hello!"}},
	})
	if err != nil {
		t.Fatalf("Chat: %v", err)
	}
	if resp.ID != "resp_123" || resp.Provider != "anthropic" {
		t.Errorf("resp = %+v", resp)
	}
	if resp.Message.Content != "Hi there!" {
		t.Errorf("content = %v", resp.Message.Content)
	}
	if resp.Reasoning == nil || *resp.Reasoning != "thinking..." {
		t.Errorf("reasoning = %v", resp.Reasoning)
	}
	if resp.Usage.TotalTokens != 15 || resp.LatencyMs != 420 {
		t.Errorf("usage/latency = %+v %d", resp.Usage, resp.LatencyMs)
	}
	if resp.Usage.CostUSD == nil || *resp.Usage.CostUSD != 1.0 || resp.Usage.CostSource != "provider_floor" {
		t.Errorf("cost accounting = %+v", resp.Usage)
	}
}

func TestChatWithConfigAndProviderConfig(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		var raw map[string]any
		if err := json.Unmarshal(body, &raw); err != nil {
			t.Fatalf("unmarshal: %v", err)
		}
		if raw["x-cache"].(map[string]any)["key"] != "session:branch" {
			t.Error("cache annotation was dropped")
		}
		if raw["x-shim"].(map[string]any)["structured_output"] != "prompt" {
			t.Error("shim option dropped")
		}
		if raw["response_format"].(map[string]any)["type"] != "json_schema" {
			t.Error("response format dropped")
		}
		cfg, ok := raw["config"].(map[string]any)
		if !ok || cfg["reasoning_effort"] != "high" {
			t.Errorf("config not sent correctly: %v", raw["config"])
		}
		if cfg["max_tokens"].(float64) != 256 {
			t.Errorf("max_tokens = %v", cfg["max_tokens"])
		}
		pc, ok := raw["provider_config"].(map[string]any)
		if !ok || pc["thinking"] == nil {
			t.Errorf("provider_config not sent: %v", raw["provider_config"])
		}
		// stream must not be serialized as true for Chat.
		if s, present := raw["stream"]; present && s == true {
			t.Errorf("stream should be false for Chat")
		}
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"id":"x","model":"m","provider":"p","message":{"role":"assistant","content":"ok"},"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2},"latency_ms":1}`)
	}))
	defer srv.Close()

	maxTok := 256
	cacheKey := "session:branch"
	c := New(WithBaseURL(srv.URL))
	_, err := c.Chat(context.Background(), ChatRequest{
		Shim:           &ShimConfig{StructuredOutput: "prompt"},
		ResponseFormat: map[string]any{"type": "json_schema", "json_schema": map[string]any{"schema": map[string]any{"type": "integer"}}},
		Cache:          &CachePolicy{Key: &cacheKey},
		Model:          "anthropic/claude-sonnet-4-6",
		Messages:       []Message{{Role: "user", Content: "hi"}},
		Config:         &Config{MaxTokens: &maxTok, ReasoningEffort: "high"},
		ProviderConfig: map[string]any{
			"thinking": map[string]any{"type": "adaptive"},
		},
	})
	if err != nil {
		t.Fatalf("Chat: %v", err)
	}
}

func TestModels(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/models" {
			t.Errorf("path = %s", r.URL.Path)
		}
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"models":[
			{"id":"anthropic/claude-sonnet-4-6","provider":"anthropic","name":"claude-sonnet-4-6"},
			{"id":"openai/gpt-5.5","provider":"openai","name":"gpt-5.5"}
		]}`)
	}))
	defer srv.Close()

	c := New(WithBaseURL(srv.URL))
	resp, err := c.Models(context.Background())
	if err != nil {
		t.Fatalf("Models: %v", err)
	}
	if len(resp.Models) != 2 {
		t.Fatalf("got %d models", len(resp.Models))
	}
	if resp.Models[0].ID != "anthropic/claude-sonnet-4-6" || resp.Models[1].Provider != "openai" {
		t.Errorf("models = %+v", resp.Models)
	}
}

func TestHealth(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/health" {
			t.Errorf("path = %s", r.URL.Path)
		}
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"status":"ok","providers":["anthropic","openai"]}`)
	}))
	defer srv.Close()

	c := New(WithBaseURL(srv.URL))
	resp, err := c.Health(context.Background())
	if err != nil {
		t.Fatalf("Health: %v", err)
	}
	if resp.Status != "ok" || len(resp.Providers) != 2 {
		t.Errorf("resp = %+v", resp)
	}
}

func TestErrorResponse(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusBadRequest)
		io.WriteString(w, `{"error":{"code":"unknown_model","message":"no such model"}}`)
	}))
	defer srv.Close()

	c := New(WithBaseURL(srv.URL))
	_, err := c.Chat(context.Background(), ChatRequest{Model: "bogus", Messages: []Message{{Role: "user", Content: "x"}}})
	if err == nil {
		t.Fatal("expected error")
	}
	apiErr, ok := err.(*APIError)
	if !ok {
		t.Fatalf("expected *APIError, got %T", err)
	}
	if apiErr.StatusCode != http.StatusBadRequest {
		t.Errorf("status = %d", apiErr.StatusCode)
	}
	if apiErr.Code != "unknown_model" || apiErr.Message != "no such model" {
		t.Errorf("apiErr = %+v", apiErr)
	}
	if !strings.Contains(apiErr.Error(), "unknown_model") {
		t.Errorf("Error() = %q", apiErr.Error())
	}
}

func TestErrorResponseNonJSON(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
		io.WriteString(w, "internal boom")
	}))
	defer srv.Close()

	c := New(WithBaseURL(srv.URL))
	_, err := c.Health(context.Background())
	apiErr, ok := err.(*APIError)
	if !ok {
		t.Fatalf("expected *APIError, got %T (%v)", err, err)
	}
	if apiErr.StatusCode != http.StatusInternalServerError || apiErr.Message != "internal boom" {
		t.Errorf("apiErr = %+v", apiErr)
	}
}

func TestStreamMultiEvent(t *testing.T) {
	sse := "event: reasoning\ndata: {\"type\":\"reasoning\",\"text\":\"let me think\"}\n\n" +
		"event: content\ndata: {\"type\":\"content\",\"text\":\"Hello\"}\n\n" +
		"event: content\ndata: {\"type\":\"content\",\"text\":\" world\"}\n\n" +
		"event: tool_call\ndata: {\"type\":\"tool_call\",\"id\":\"call_1\",\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}\n\n" +
		"event: usage\ndata: {\"type\":\"usage\",\"input_tokens\":10,\"output_tokens\":20,\"total_tokens\":30}\n\n" +
		"event: done\ndata: {\"type\":\"done\"}\n\n"

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/chat/stream" {
			t.Errorf("path = %s", r.URL.Path)
		}
		w.Header().Set("Content-Type", "text/event-stream")
		io.WriteString(w, sse)
	}))
	defer srv.Close()

	c := New(WithBaseURL(srv.URL))
	ch, err := c.Stream(context.Background(), ChatRequest{
		Model:    "anthropic/claude-sonnet-4-6",
		Messages: []Message{{Role: "user", Content: "hi"}},
	})
	if err != nil {
		t.Fatalf("Stream: %v", err)
	}

	var events []StreamEvent
	for ev := range ch {
		if ev.Err != nil {
			t.Fatalf("stream error: %v", ev.Err)
		}
		events = append(events, ev)
	}

	if len(events) != 6 {
		t.Fatalf("got %d events: %+v", len(events), events)
	}
	if events[0].Type != EventReasoning || events[0].Text != "let me think" {
		t.Errorf("event 0 = %+v", events[0])
	}
	var text string
	for _, ev := range events {
		if ev.Type == EventContent {
			text += ev.Text
		}
	}
	if text != "Hello world" {
		t.Errorf("assembled text = %q", text)
	}
	tc := events[3]
	if tc.Type != EventToolCall || tc.ID != "call_1" || tc.Name != "get_weather" || tc.Arguments != `{"city":"SF"}` {
		t.Errorf("tool_call = %+v", tc)
	}
	u := events[4]
	if u.Type != EventUsage || u.InputTokens != 10 || u.OutputTokens != 20 || u.TotalTokens != 30 {
		t.Errorf("usage = %+v", u)
	}
	if events[5].Type != EventDone {
		t.Errorf("last event = %+v", events[5])
	}
}

func TestStreamErrorEvent(t *testing.T) {
	sse := "event: error\ndata: {\"type\":\"error\",\"message\":\"rate limited\"}\n\n"
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		io.WriteString(w, sse)
	}))
	defer srv.Close()

	c := New(WithBaseURL(srv.URL))
	ch, err := c.Stream(context.Background(), ChatRequest{Model: "m", Messages: []Message{{Role: "user", Content: "x"}}})
	if err != nil {
		t.Fatalf("Stream: %v", err)
	}
	var events []StreamEvent
	for ev := range ch {
		events = append(events, ev)
	}
	if len(events) != 1 || events[0].Type != EventError || events[0].Message != "rate limited" {
		t.Errorf("events = %+v", events)
	}
}

func TestStreamNon2xx(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusBadRequest)
		io.WriteString(w, `{"error":{"code":"bad_request","message":"missing model"}}`)
	}))
	defer srv.Close()

	c := New(WithBaseURL(srv.URL))
	_, err := c.Stream(context.Background(), ChatRequest{Messages: []Message{{Role: "user", Content: "x"}}})
	apiErr, ok := err.(*APIError)
	if !ok {
		t.Fatalf("expected *APIError, got %T (%v)", err, err)
	}
	if apiErr.Code != "bad_request" {
		t.Errorf("apiErr = %+v", apiErr)
	}
}

func TestStreamNoTrailingBlankLine(t *testing.T) {
	// Final event without a trailing blank line should still be dispatched.
	sse := "event: content\ndata: {\"type\":\"content\",\"text\":\"tail\"}"
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		io.WriteString(w, sse)
	}))
	defer srv.Close()

	c := New(WithBaseURL(srv.URL))
	ch, err := c.Stream(context.Background(), ChatRequest{Model: "m", Messages: []Message{{Role: "user", Content: "x"}}})
	if err != nil {
		t.Fatalf("Stream: %v", err)
	}
	var events []StreamEvent
	for ev := range ch {
		events = append(events, ev)
	}
	if len(events) != 1 || events[0].Text != "tail" {
		t.Errorf("events = %+v", events)
	}
}

func TestWithHeader(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer secret" {
			t.Errorf("missing auth header: %q", r.Header.Get("Authorization"))
		}
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"status":"ok","providers":[]}`)
	}))
	defer srv.Close()

	c := New(WithBaseURL(srv.URL), WithHeader("Authorization", "Bearer secret"))
	if _, err := c.Health(context.Background()); err != nil {
		t.Fatalf("Health: %v", err)
	}
}

func TestReasoningProvenanceSurvivesTypedRoundTrip(t *testing.T) {
	raw := `{"role":"assistant","content":null,"reasoning":[{"kind":"encrypted","data":"opaque+/=","item_id":"rs_1","origin":{"provider":"openai","model":"gpt-6-astra","family":"gpt","wire":"openai-responses","received_at":"2026-09-16T00:00:00Z","account":"issuer"},"payload":{"type":"reasoning","summary":[],"encrypted_content":"opaque+/="}}],"tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"},"thought_signature":{"data":"sig","origin":{"provider":"gemini","model":"gemini-3.8-flash","family":"gemini","wire":"google-generate-content","received_at":"2026-09-16T00:00:00Z"}}}]}`
	var message ResponseMessage
	if err := json.Unmarshal([]byte(raw), &message); err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(message)
	if err != nil {
		t.Fatal(err)
	}
	var before, after any
	if err := json.Unmarshal([]byte(raw), &before); err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(encoded, &after); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(before, after) {
		t.Fatalf("metadata was lost: %s", encoded)
	}
}

func TestWireIDsSurviveTypedRoundTrip(t *testing.T) {
	raw := `{"role":"assistant","content":null,"tool_calls":[{"id":"call_ls_example","type":"function","function":{"name":"read","arguments":"{}"},"wire_ids":[{"provider":"google-compatible","wire":"openai-chat","scope":"r1","part_id":"0","id":"native_call","signature_field":"extra_content.google.thought_signature"},{"provider":"gemini","wire":"google-generate-content","scope":"r2","part_id":"1","id":null},{"provider":"openai","wire":"openai-responses","scope":"r3","part_id":"2","id":"call_1","item_id":"fc_1"}]}]}`
	var message ResponseMessage
	if err := json.Unmarshal([]byte(raw), &message); err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(message)
	if err != nil {
		t.Fatal(err)
	}
	var before, after any
	if err := json.Unmarshal([]byte(raw), &before); err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(encoded, &after); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(before, after) {
		t.Fatalf("wire identity was lost: %s", encoded)
	}
}
