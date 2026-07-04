// Package llmshim is a thin, idiomatic Go client for the llmshim proxy.
//
// It speaks pure HTTP to a running llmshim proxy (default
// http://localhost:3000) and does not spawn the Rust binary.
//
//	client := llmshim.New()
//	resp, err := client.Chat(ctx, llmshim.ChatRequest{
//		Model:    "anthropic/claude-sonnet-4-6",
//		Messages: []llmshim.Message{{Role: "user", Content: "Hello!"}},
//	})
package llmshim

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"strings"
	"time"
)

// DefaultBaseURL is used when no base URL is configured.
const DefaultBaseURL = "http://localhost:3000"

// Client talks to an llmshim proxy over HTTP. It is safe for concurrent use.
type Client struct {
	baseURL    string
	httpClient *http.Client
	headers    map[string]string
}

// Option configures a Client.
type Option func(*Client)

// WithBaseURL sets the proxy base URL (default http://localhost:3000).
// A trailing slash is trimmed.
func WithBaseURL(baseURL string) Option {
	return func(c *Client) {
		c.baseURL = strings.TrimRight(baseURL, "/")
	}
}

// WithHTTPClient sets a custom *http.Client (for custom timeouts, transports,
// or proxies).
func WithHTTPClient(hc *http.Client) Option {
	return func(c *Client) {
		if hc != nil {
			c.httpClient = hc
		}
	}
}

// WithHeader adds a header sent on every request (e.g. Authorization).
func WithHeader(key, value string) Option {
	return func(c *Client) {
		c.headers[key] = value
	}
}

// New creates a Client. With no options it targets DefaultBaseURL with a
// 120-second timeout.
func New(opts ...Option) *Client {
	c := &Client{
		baseURL:    DefaultBaseURL,
		httpClient: &http.Client{Timeout: 120 * time.Second},
		headers:    make(map[string]string),
	}
	for _, opt := range opts {
		opt(c)
	}
	return c
}

// newRequest builds an *http.Request with the JSON body (if any) and headers.
func (c *Client) newRequest(ctx context.Context, method, path string, body any) (*http.Request, error) {
	var reader io.Reader
	if body != nil {
		data, err := json.Marshal(body)
		if err != nil {
			return nil, err
		}
		reader = bytes.NewReader(data)
	}

	req, err := http.NewRequestWithContext(ctx, method, c.baseURL+path, reader)
	if err != nil {
		return nil, err
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	req.Header.Set("Accept", "application/json")
	for k, v := range c.headers {
		req.Header.Set(k, v)
	}
	return req, nil
}

// doJSON performs a request and decodes a JSON response into out.
func (c *Client) doJSON(ctx context.Context, method, path string, body, out any) error {
	req, err := c.newRequest(ctx, method, path, body)
	if err != nil {
		return err
	}
	resp, err := c.httpClient.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode/100 != 2 {
		return parseError(resp)
	}
	if out == nil {
		return nil
	}
	return json.NewDecoder(resp.Body).Decode(out)
}

// parseError reads a non-2xx response body into an *APIError.
func parseError(resp *http.Response) error {
	data, _ := io.ReadAll(resp.Body)
	apiErr := &APIError{StatusCode: resp.StatusCode}

	var er ErrorResponse
	if err := json.Unmarshal(data, &er); err == nil && er.Error.Message != "" {
		apiErr.Code = er.Error.Code
		apiErr.Message = er.Error.Message
		return apiErr
	}

	apiErr.Message = strings.TrimSpace(string(data))
	if apiErr.Message == "" {
		apiErr.Message = resp.Status
	}
	return apiErr
}

// Chat sends a non-streaming chat completion request to POST /v1/chat.
func (c *Client) Chat(ctx context.Context, req ChatRequest) (*ChatResponse, error) {
	req.Stream = false
	var out ChatResponse
	if err := c.doJSON(ctx, http.MethodPost, "/v1/chat", req, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// Models lists the models available on the proxy (GET /v1/models).
func (c *Client) Models(ctx context.Context) (*ModelsResponse, error) {
	var out ModelsResponse
	if err := c.doJSON(ctx, http.MethodGet, "/v1/models", nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// Health returns the proxy health status (GET /health).
func (c *Client) Health(ctx context.Context) (*HealthResponse, error) {
	var out HealthResponse
	if err := c.doJSON(ctx, http.MethodGet, "/health", nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// Stream sends a streaming chat completion request to POST /v1/chat/stream and
// returns a channel of typed events. The channel is closed when the stream
// ends (a "done" event or EOF). If a transport or parse error occurs while
// reading, a StreamEvent with Err set is sent before the channel closes.
//
// The returned error is non-nil only if the initial request fails (transport
// error or non-2xx status). Cancel ctx to stop consuming early.
func (c *Client) Stream(ctx context.Context, req ChatRequest) (<-chan StreamEvent, error) {
	req.Stream = true
	httpReq, err := c.newRequest(ctx, http.MethodPost, "/v1/chat/stream", req)
	if err != nil {
		return nil, err
	}
	httpReq.Header.Set("Accept", "text/event-stream")

	resp, err := c.httpClient.Do(httpReq)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode/100 != 2 {
		defer resp.Body.Close()
		return nil, parseError(resp)
	}

	ch := make(chan StreamEvent)
	go func() {
		defer close(ch)
		defer resp.Body.Close()
		parseSSE(ctx, resp.Body, ch)
	}()
	return ch, nil
}

// parseSSE reads server-sent events from r and forwards typed events to ch.
func parseSSE(ctx context.Context, r io.Reader, ch chan<- StreamEvent) {
	reader := bufio.NewReader(r)
	var eventType string
	var dataBuf strings.Builder

	send := func(ev StreamEvent) bool {
		select {
		case ch <- ev:
			return true
		case <-ctx.Done():
			return false
		}
	}

	// dispatch emits the buffered event, returning (stop, ok) where stop
	// indicates a terminal "done" event and ok reports whether sending
	// succeeded (false means ctx was cancelled).
	dispatch := func() (stop, ok bool) {
		if dataBuf.Len() == 0 && eventType == "" {
			return false, true
		}
		data := dataBuf.String()
		dataBuf.Reset()
		et := eventType
		eventType = ""

		var ev StreamEvent
		if data != "" {
			if err := json.Unmarshal([]byte(data), &ev); err != nil {
				return false, send(StreamEvent{Type: EventError, Err: err, Message: data})
			}
		}
		if ev.Type == "" {
			ev.Type = StreamEventType(et)
		}
		if ev.Type == EventDone {
			return true, send(ev)
		}
		return false, send(ev)
	}

	for {
		line, err := reader.ReadString('\n')
		if len(line) > 0 {
			trimmed := strings.TrimRight(line, "\r\n")
			switch {
			case trimmed == "":
				// Blank line: end of one event.
				if stop, ok := dispatch(); stop || !ok {
					return
				}
			case strings.HasPrefix(trimmed, ":"):
				// Comment line; ignore.
			case strings.HasPrefix(trimmed, "event:"):
				eventType = strings.TrimSpace(trimmed[len("event:"):])
			case strings.HasPrefix(trimmed, "data:"):
				val := trimmed[len("data:"):]
				val = strings.TrimPrefix(val, " ")
				if dataBuf.Len() > 0 {
					dataBuf.WriteByte('\n')
				}
				dataBuf.WriteString(val)
			}
		}

		if err != nil {
			// Flush any trailing event, then finish.
			dispatch()
			if !errors.Is(err, io.EOF) {
				send(StreamEvent{Type: EventError, Err: err})
			}
			return
		}
	}
}
