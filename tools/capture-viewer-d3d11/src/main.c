/* Windows D3D11 reference viewer for Jackstay (ABI 0.10).
 *
 * Connects to a Local Endpoint, asks the producer for its adapter, creates its
 * own D3D11 device on that LUID, attaches, registers a shared release fence
 * and presents frames into a flip-model swap chain. Each frame's texture and
 * readiness fence are imported from borrowed NT handles once per pool slot and
 * once per fence, the draw GPU-waits the frame's fence value, and the frame is
 * released to the viewer's own fence value signalled after the draw. The CPU
 * never waits for the producer.
 *
 *   capture-viewer-d3d11 --endpoint NAME [--session-scope] [--frames N]
 *                        [--hold-ms MS] [--holding N]
 *
 * Plain Win32 + D3D11 rather than SDL: SDL2's D3D11 renderer cannot choose an
 * adapter by LUID or sample an external shared texture, both of which this
 * path needs. See README.md. */
#define COBJMACROS
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_4.h>
#include <d3dcompiler.h>
#include <dxgi1_2.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "capture_transfer.h"

#define MAX_CACHED_TEXTURES 32

typedef struct viewer_options {
  const char *endpoint;
  uint32_t scope;
  long max_frames;
  uint32_t hold_ms;
  uint32_t holding;
} viewer_options;

typedef struct cached_texture {
  uint64_t pool_id;
  uint32_t slot_id;
  ID3D11Texture2D *texture;
  ID3D11ShaderResourceView *view;
} cached_texture;

typedef struct viewer {
  HWND window;
  int running;
  int resized;
  IDXGIFactory2 *factory;
  ID3D11Device *device;
  ID3D11Device1 *device1;
  ID3D11Device5 *device5;
  ID3D11DeviceContext *context;
  ID3D11DeviceContext4 *context4;
  IDXGISwapChain1 *swap_chain;
  ID3D11RenderTargetView *target;
  ID3D11VertexShader *vertex_shader;
  ID3D11PixelShader *pixel_shader;
  ID3D11SamplerState *sampler;
  ID3D11Fence *release_fence;
  uint64_t release_value;
  cached_texture textures[MAX_CACHED_TEXTURES];
  uint64_t pool_id;
  uint64_t readiness_id;
  ID3D11Fence *readiness;
} viewer;

#define RELEASE(object)                   \
  do {                                    \
    if ((object) != NULL) {               \
      IUnknown_Release((IUnknown *)(object)); \
      (object) = NULL;                    \
    }                                     \
  } while (0)

static const char SHADERS[] =
    "Texture2D frame_texture : register(t0);\n"
    "SamplerState frame_sampler : register(s0);\n"
    "struct v2p { float4 position : SV_Position; float2 uv : TEXCOORD0; };\n"
    "v2p vs_main(uint id : SV_VertexID) {\n"
    "  v2p o; float2 uv = float2((id << 1) & 2, id & 2);\n"
    "  o.uv = uv; o.position = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1); return o;\n"
    "}\n"
    "float4 ps_main(v2p i) : SV_Target { return float4(frame_texture.Sample(frame_sampler, i.uv).rgb, 1); }\n";

static int parse_u32(const char *text, uint32_t *out) {
  if (text == NULL || text[0] < '0' || text[0] > '9') return 0;
  char *end = NULL;
  unsigned long long value = strtoull(text, &end, 10);
  if (*end != '\0' || value > UINT32_MAX) return 0;
  *out = (uint32_t)value;
  return 1;
}

static int parse_options(int argc, char **argv, viewer_options *options) {
  *options = (viewer_options){.scope = FT_ENDPOINT_SCOPE_USER, .holding = 2};
  for (int i = 1; i < argc; i++) {
    uint32_t value = 0;
    if (strcmp(argv[i], "--endpoint") == 0 && i + 1 < argc && argv[i + 1][0]) {
      options->endpoint = argv[++i];
    } else if (strcmp(argv[i], "--session-scope") == 0) {
      options->scope = FT_ENDPOINT_SCOPE_SESSION;
    } else if (strcmp(argv[i], "--frames") == 0 && i + 1 < argc && parse_u32(argv[i + 1], &value)) {
      options->max_frames = (long)value;
      i++;
    } else if (strcmp(argv[i], "--hold-ms") == 0 && i + 1 < argc && parse_u32(argv[i + 1], &value)) {
      options->hold_ms = value;
      i++;
    } else if (strcmp(argv[i], "--holding") == 0 && i + 1 < argc && parse_u32(argv[i + 1], &value) && value) {
      options->holding = value;
      i++;
    } else {
      fprintf(stderr, "unknown or incomplete argument %s\n", argv[i]);
      return 0;
    }
  }
  if (options->endpoint == NULL) {
    fprintf(stderr, "usage: capture-viewer-d3d11 --endpoint NAME [--session-scope] [--frames N] [--hold-ms MS] [--holding N]\n");
    return 0;
  }
  return 1;
}

static LRESULT CALLBACK window_procedure(HWND window, UINT message, WPARAM wparam, LPARAM lparam) {
  viewer *state = (viewer *)GetWindowLongPtrW(window, GWLP_USERDATA);
  switch (message) {
  case WM_SIZE:
    if (state != NULL) state->resized = 1;
    return 0;
  case WM_CLOSE:
    if (state != NULL) state->running = 0;
    return 0;
  default:
    return DefWindowProcW(window, message, wparam, lparam);
  }
}

static void pump(void) {
  MSG message;
  while (PeekMessageW(&message, NULL, 0, 0, PM_REMOVE)) {
    TranslateMessage(&message);
    DispatchMessageW(&message);
  }
}

static int check(HRESULT result, const char *operation) {
  if (SUCCEEDED(result)) return 0;
  fprintf(stderr, "%s failed: 0x%08lx\n", operation, (unsigned long)result);
  return 1;
}

static int require_ok(ft_status status, const char *operation) {
  if (status == FT_STATUS_OK) return 0;
  fprintf(stderr, "%s failed with status %d\n", operation, status);
  return 1;
}

/* A device on exactly the producer's adapter: shared D3D11 resources import
 * only there, and the producer refuses any other. */
static int create_device(viewer *state, uint64_t luid) {
  IDXGIFactory1 *factory1 = NULL;
  IDXGIAdapter1 *chosen = NULL;
  if (check(CreateDXGIFactory1(&IID_IDXGIFactory1, (void **)&factory1), "CreateDXGIFactory1")) return 1;
  for (UINT index = 0;; index++) {
    IDXGIAdapter1 *adapter = NULL;
    if (FAILED(IDXGIFactory1_EnumAdapters1(factory1, index, &adapter))) break;
    DXGI_ADAPTER_DESC1 desc;
    if (SUCCEEDED(IDXGIAdapter1_GetDesc1(adapter, &desc)) &&
        (((uint64_t)(uint32_t)desc.AdapterLuid.HighPart << 32) | desc.AdapterLuid.LowPart) == luid) {
      chosen = adapter;
      break;
    }
    IDXGIAdapter1_Release(adapter);
  }
  int failed = 1;
  if (chosen == NULL) {
    fprintf(stderr, "no DXGI adapter has the producer's LUID %016" PRIx64 "\n", luid);
    goto cleanup;
  }
  D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
  if (check(D3D11CreateDevice((IDXGIAdapter *)chosen, D3D_DRIVER_TYPE_UNKNOWN, NULL, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                              levels, 2, D3D11_SDK_VERSION, &state->device, NULL, &state->context),
            "D3D11CreateDevice") ||
      check(ID3D11Device_QueryInterface(state->device, &IID_ID3D11Device1, (void **)&state->device1), "ID3D11Device1") ||
      check(ID3D11Device_QueryInterface(state->device, &IID_ID3D11Device5, (void **)&state->device5), "ID3D11Device5") ||
      check(ID3D11DeviceContext_QueryInterface(state->context, &IID_ID3D11DeviceContext4, (void **)&state->context4),
            "ID3D11DeviceContext4") ||
      check(IDXGIFactory1_QueryInterface(factory1, &IID_IDXGIFactory2, (void **)&state->factory), "IDXGIFactory2"))
    goto cleanup;
  failed = 0;
cleanup:
  RELEASE(chosen);
  RELEASE(factory1);
  return failed;
}

static int create_target(viewer *state) {
  ID3D11Texture2D *back_buffer = NULL;
  if (check(IDXGISwapChain1_GetBuffer(state->swap_chain, 0, &IID_ID3D11Texture2D, (void **)&back_buffer), "GetBuffer")) return 1;
  HRESULT result = ID3D11Device_CreateRenderTargetView(state->device, (ID3D11Resource *)back_buffer, NULL, &state->target);
  RELEASE(back_buffer);
  return check(result, "CreateRenderTargetView");
}

static int create_presentation(viewer *state) {
  DXGI_SWAP_CHAIN_DESC1 desc = {0};
  desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  desc.SampleDesc.Count = 1;
  desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
  desc.BufferCount = 2;
  desc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
  if (check(IDXGIFactory2_CreateSwapChainForHwnd(state->factory, (IUnknown *)state->device, state->window, &desc, NULL, NULL,
                                                 &state->swap_chain),
            "CreateSwapChainForHwnd") ||
      create_target(state))
    return 1;
  ID3DBlob *vertex = NULL, *pixel = NULL, *errors = NULL;
  int failed = 1;
  if (FAILED(D3DCompile(SHADERS, sizeof SHADERS - 1, "viewer", NULL, NULL, "vs_main", "vs_4_0", 0, 0, &vertex, &errors)) ||
      FAILED(D3DCompile(SHADERS, sizeof SHADERS - 1, "viewer", NULL, NULL, "ps_main", "ps_4_0", 0, 0, &pixel, &errors))) {
    fprintf(stderr, "shader compilation failed: %s\n", errors ? (const char *)ID3D10Blob_GetBufferPointer(errors) : "");
    goto cleanup;
  }
  D3D11_SAMPLER_DESC sampler = {0};
  sampler.Filter = D3D11_FILTER_MIN_MAG_MIP_LINEAR;
  sampler.AddressU = sampler.AddressV = sampler.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
  sampler.MaxLOD = D3D11_FLOAT32_MAX;
  if (check(ID3D11Device_CreateVertexShader(state->device, ID3D10Blob_GetBufferPointer(vertex), ID3D10Blob_GetBufferSize(vertex),
                                            NULL, &state->vertex_shader),
            "CreateVertexShader") ||
      check(ID3D11Device_CreatePixelShader(state->device, ID3D10Blob_GetBufferPointer(pixel), ID3D10Blob_GetBufferSize(pixel), NULL,
                                           &state->pixel_shader),
            "CreatePixelShader") ||
      check(ID3D11Device_CreateSamplerState(state->device, &sampler, &state->sampler), "CreateSamplerState"))
    goto cleanup;
  failed = 0;
cleanup:
  RELEASE(vertex);
  RELEASE(pixel);
  RELEASE(errors);
  return failed;
}

static int resize_target(viewer *state) {
  ID3D11DeviceContext_OMSetRenderTargets(state->context, 0, NULL, NULL);
  RELEASE(state->target);
  if (check(IDXGISwapChain1_ResizeBuffers(state->swap_chain, 0, 0, 0, DXGI_FORMAT_UNKNOWN, 0), "ResizeBuffers")) return 1;
  return create_target(state);
}

/* Size the window's client area to the frame, scaled down to fit 85% of the
 * work area, so the presented aspect is the frame's. */
static void fit_window(viewer *state, uint32_t width, uint32_t height) {
  RECT work;
  if (!SystemParametersInfoW(SPI_GETWORKAREA, 0, &work, 0)) return;
  double scale = 1.0, max_width = (work.right - work.left) * 0.85, max_height = (work.bottom - work.top) * 0.85;
  if (width * scale > max_width) scale = max_width / width;
  if (height * scale > max_height) scale = max_height / height;
  RECT rect = {0, 0, (LONG)(width * scale), (LONG)(height * scale)};
  if (rect.right < 64) rect.right = 64;
  if (rect.bottom < 64) rect.bottom = 64;
  AdjustWindowRectEx(&rect, WS_OVERLAPPEDWINDOW, FALSE, 0);
  SetWindowPos(state->window, NULL, 0, 0, rect.right - rect.left, rect.bottom - rect.top,
               SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
}

static void forget_pools_except(viewer *state, uint64_t pool_id) {
  for (int i = 0; i < MAX_CACHED_TEXTURES; i++) {
    cached_texture *entry = &state->textures[i];
    if (entry->texture != NULL && entry->pool_id != pool_id) {
      /* D3D11 keeps a resource alive for GPU work already queued on it. */
      RELEASE(entry->view);
      RELEASE(entry->texture);
    }
  }
}

/* Import once per pool slot; the view is cached with it. Frames arrive in
 * cursor order and the viewer holds none across iterations, so the first frame
 * of a new pool ends the use of older pools' imports. */
static ID3D11ShaderResourceView *import_texture(viewer *state, const ft_acquired_frame_descriptor *descriptor, void *handle) {
  if (descriptor->pool_id != state->pool_id) {
    forget_pools_except(state, descriptor->pool_id);
    state->pool_id = descriptor->pool_id;
  }
  cached_texture *free_entry = NULL;
  for (int i = 0; i < MAX_CACHED_TEXTURES; i++) {
    cached_texture *entry = &state->textures[i];
    if (entry->texture != NULL && entry->pool_id == descriptor->pool_id && entry->slot_id == descriptor->slot_id) return entry->view;
    if (entry->texture == NULL && free_entry == NULL) free_entry = entry;
  }
  if (free_entry == NULL) {
    fprintf(stderr, "more than %d pool slots\n", MAX_CACHED_TEXTURES);
    return NULL;
  }
  if (check(ID3D11Device1_OpenSharedResource1(state->device1, (HANDLE)handle, &IID_ID3D11Texture2D, (void **)&free_entry->texture),
            "OpenSharedResource1"))
    return NULL;
  if (check(ID3D11Device_CreateShaderResourceView(state->device, (ID3D11Resource *)free_entry->texture, NULL, &free_entry->view),
            "CreateShaderResourceView")) {
    RELEASE(free_entry->texture);
    return NULL;
  }
  free_entry->pool_id = descriptor->pool_id;
  free_entry->slot_id = descriptor->slot_id;
  return free_entry->view;
}

/* Import once per producer fence. */
static ID3D11Fence *import_readiness(viewer *state, const ft_acquired_frame_descriptor *descriptor, void *handle) {
  if (state->readiness != NULL && state->readiness_id == descriptor->fence_id) return state->readiness;
  RELEASE(state->readiness);
  if (check(ID3D11Device5_OpenSharedFence(state->device5, (HANDLE)handle, &IID_ID3D11Fence, (void **)&state->readiness),
            "OpenSharedFence"))
    return NULL;
  state->readiness_id = descriptor->fence_id;
  return state->readiness;
}

static void draw(viewer *state, ID3D11ShaderResourceView *view, uint32_t width, uint32_t height) {
  RECT client;
  GetClientRect(state->window, &client);
  float client_width = (float)(client.right - client.left), client_height = (float)(client.bottom - client.top);
  float scale = client_width / width < client_height / height ? client_width / width : client_height / height;
  D3D11_VIEWPORT viewport = {(client_width - width * scale) / 2, (client_height - height * scale) / 2, width * scale,
                             height * scale, 0, 1};
  const float background[4] = {0.08f, 0.08f, 0.08f, 1};
  ID3D11DeviceContext *context = state->context;
  ID3D11DeviceContext_ClearRenderTargetView(context, state->target, background);
  ID3D11DeviceContext_OMSetRenderTargets(context, 1, &state->target, NULL);
  ID3D11DeviceContext_RSSetViewports(context, 1, &viewport);
  ID3D11DeviceContext_IASetInputLayout(context, NULL);
  ID3D11DeviceContext_IASetPrimitiveTopology(context, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
  ID3D11DeviceContext_VSSetShader(context, state->vertex_shader, NULL, 0);
  ID3D11DeviceContext_PSSetShader(context, state->pixel_shader, NULL, 0);
  ID3D11DeviceContext_PSSetShaderResources(context, 0, 1, &view);
  ID3D11DeviceContext_PSSetSamplers(context, 0, 1, &state->sampler);
  ID3D11DeviceContext_Draw(context, 3, 0);
  ID3D11ShaderResourceView *none = NULL;
  ID3D11DeviceContext_PSSetShaderResources(context, 0, 1, &none);
}

/* Keep the lease while deliberately delaying consumption; the window stays
 * responsive and closing it ends the hold. */
static int hold_frame(viewer *state, uint32_t remaining_ms) {
  while (remaining_ms != 0 && state->running) {
    uint32_t chunk = remaining_ms < 16 ? remaining_ms : 16;
    Sleep(chunk);
    remaining_ms -= chunk;
    pump();
  }
  return state->running;
}

/* Wait for the viewer's own GPU work, which covers every deferred release. */
static int drain(viewer *state, DWORD timeout_ms) {
  if (state->release_fence == NULL || state->release_value == 0) return 1;
  if (ID3D11Fence_GetCompletedValue(state->release_fence) >= state->release_value) return 1;
  HANDLE event = CreateEventW(NULL, FALSE, FALSE, NULL);
  if (event == NULL) return 0;
  int drained = SUCCEEDED(ID3D11Fence_SetEventOnCompletion(state->release_fence, state->release_value, event)) &&
                WaitForSingleObject(event, timeout_ms) == WAIT_OBJECT_0;
  CloseHandle(event);
  return drained;
}

int main(int argc, char **argv) {
  if (ft_abi_version() != FT_ABI_VERSION) {
    fprintf(stderr, "Jackstay ABI mismatch: rebuild the viewer and library together\n");
    return 1;
  }
  viewer_options options;
  if (!parse_options(argc, argv, &options)) return 1;
  SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

  viewer state = {0};
  ft_local_connection *local = NULL;
  ft_d3d11_acquisition_connection *connection = NULL;
  ft_acquisition_consumer *consumer = NULL;
  ft_acquisition_release_timeline *timeline = NULL;
  ft_acquisition_cancellation *cancellation = NULL;
  int failed = 1, producer_lost = 0;
  uint64_t presented = 0;

  ft_local_endpoint endpoint = {options.scope, FT_ENDPOINT_TRANSPORT_LOCAL_STREAM, options.endpoint};
  ft_d3d11_adapter adapter = {0};
  if (require_ok(ft_local_connect(&endpoint, &local), "ft_local_connect") ||
      require_ok(ft_acquisition_d3d11_connection_create_local(&local, &connection), "ft_acquisition_d3d11_connection_create_local") ||
      require_ok(ft_acquisition_d3d11_describe(connection, &adapter), "ft_acquisition_d3d11_describe"))
    goto cleanup;
  printf("producer adapter=%016" PRIx64 " description=\"%s\" software=%u\n", adapter.luid, adapter.description, adapter.software);
  if (create_device(&state, adapter.luid) ||
      require_ok(ft_acquisition_d3d11_attach(connection, state.device, options.holding, &consumer), "ft_acquisition_d3d11_attach") ||
      check(ID3D11Device5_CreateFence(state.device5, 0, D3D11_FENCE_FLAG_SHARED, &IID_ID3D11Fence, (void **)&state.release_fence),
            "CreateFence") ||
      require_ok(ft_acquisition_d3d11_register_release(connection, consumer, state.release_fence, &timeline),
                 "ft_acquisition_d3d11_register_release") ||
      require_ok(ft_acquisition_cancellation_create(&cancellation), "ft_acquisition_cancellation_create"))
    goto cleanup;

  WNDCLASSW window_class = {0};
  window_class.lpfnWndProc = window_procedure;
  window_class.hInstance = GetModuleHandleW(NULL);
  window_class.hCursor = LoadCursorW(NULL, (LPCWSTR)IDC_ARROW);
  window_class.lpszClassName = L"JackstayCaptureViewerD3D11";
  RegisterClassW(&window_class);
  state.window = CreateWindowExW(0, window_class.lpszClassName, L"capture-viewer-d3d11", WS_OVERLAPPEDWINDOW, CW_USEDEFAULT,
                                 CW_USEDEFAULT, 640, 360, NULL, NULL, window_class.hInstance, NULL);
  if (state.window == NULL) {
    fprintf(stderr, "CreateWindowExW failed: %lu\n", GetLastError());
    goto cleanup;
  }
  SetWindowLongPtrW(state.window, GWLP_USERDATA, (LONG_PTR)&state);
  if (create_presentation(&state)) goto cleanup;
  ShowWindow(state.window, SW_SHOWNOACTIVATE);
  printf("viewer window=%p\n", (void *)state.window);
  fflush(stdout);

  failed = 0;
  state.running = 1;
  uint64_t last_cursor = 0, requested_epoch = 0;
  int requested_configuration = 0;
  uint32_t shown_width = 0, shown_height = 0;
  while (state.running && (options.max_frames <= 0 || presented < (uint64_t)options.max_frames)) {
    pump();
    if (!state.running) break;
    if (state.resized) {
      state.resized = 0;
      if (resize_target(&state)) { failed = 1; break; }
    }
    ft_status alive = ft_acquisition_d3d11_connection_alive(connection);
    if (alive != FT_STATUS_OK) {
      printf("producer setup connection %s\n", alive == FT_STATUS_CLOSED ? "closed" : "failed");
      producer_lost = 1;
      break;
    }
    /* Snapshot before acquisition so publication, release or reconfiguration
     * between the check and the wait cannot be lost. */
    ft_acquisition_events before = {0};
    if (require_ok(ft_acquisition_snapshot(consumer, &before), "ft_acquisition_snapshot")) { failed = 1; break; }
    ft_acquired_frame *frame = NULL;
    ft_acquisition_range range = {0};
    ft_status status = ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, last_cursor, &frame, &range);
    uint32_t interest = FT_WAIT_DATA;
    if (status == FT_STATUS_OK) {
      ft_acquired_frame_descriptor descriptor = {0};
      void *texture_handle = NULL, *readiness_handle = NULL;
      if (require_ok(ft_acquired_frame_describe(frame, &descriptor), "ft_acquired_frame_describe") ||
          require_ok(ft_acquired_frame_d3d11_resources(frame, &texture_handle, &readiness_handle), "ft_acquired_frame_d3d11_resources")) {
        ft_acquired_frame_release(&frame);
        failed = 1;
        break;
      }
      /* Import while the frame holds the handles. */
      ID3D11ShaderResourceView *view = import_texture(&state, &descriptor, texture_handle);
      ID3D11Fence *readiness = import_readiness(&state, &descriptor, readiness_handle);
      if (view == NULL || readiness == NULL) {
        ft_acquired_frame_release(&frame);
        failed = 1;
        break;
      }
      if (ft_d3d11_fence_alive(readiness) == FT_STATUS_CLOSED) {
        /* The copy behind this frame never ran: discard it. */
        printf("producer lost: readiness fence abandoned\n");
        ft_acquired_frame_release(&frame);
        producer_lost = 1;
        break;
      }
      if (!hold_frame(&state, options.hold_ms)) {
        ft_acquired_frame_release(&frame);
        break;
      }
      if (descriptor.width != shown_width || descriptor.height != shown_height) {
        fit_window(&state, descriptor.width, descriptor.height);
        pump();
        if (state.resized) {
          state.resized = 0;
          if (resize_target(&state)) { ft_acquired_frame_release(&frame); failed = 1; break; }
        }
        shown_width = descriptor.width;
        shown_height = descriptor.height;
        printf("frame %ux%u generation %" PRIu64 " pool %" PRIu64 "\n", descriptor.width, descriptor.height,
               descriptor.config_generation, descriptor.pool_id);
        fflush(stdout);
      }
      /* GPU-wait the producer's copy, draw, then signal our release fence on
       * the same context so its value covers this frame's last use. */
      if (check(ID3D11DeviceContext4_Wait(state.context4, readiness, descriptor.fence_value), "Wait")) {
        ft_acquired_frame_release(&frame);
        failed = 1;
        break;
      }
      draw(&state, view, descriptor.width, descriptor.height);
      HRESULT presented_result = IDXGISwapChain1_Present(state.swap_chain, 1, 0);
      state.release_value++;
      if (check(ID3D11DeviceContext4_Signal(state.context4, state.release_fence, state.release_value), "Signal")) {
        /* Nothing will signal this value and the draw may still be pending:
         * keep the frame rather than declare completion early. Process exit
         * leaves its reclamation to the producer's cleanup. */
        ID3D11DeviceContext_Flush(state.context);
        failed = 1;
        break;
      }
      ID3D11DeviceContext_Flush(state.context);
      if (require_ok(ft_acquired_frame_defer_release(&frame, timeline, state.release_value), "ft_acquired_frame_defer_release")) {
        drain(&state, 5000);
        ft_acquired_frame_release(&frame);
        failed = 1;
        break;
      }
      if (check(presented_result, "Present")) { failed = 1; break; }
      last_cursor = descriptor.cursor;
      presented++;
      continue;
    }
    if (status == FT_STATUS_CLOSED) break;
    if (status == FT_STATUS_RECONFIGURATION) {
      interest = FT_WAIT_ALL;
      if (!requested_configuration || requested_epoch != before.reconfiguration_epoch) {
        requested_configuration = 1;
        requested_epoch = before.reconfiguration_epoch;
        if (require_ok(ft_acquisition_relinquish_configuration(consumer), "ft_acquisition_relinquish_configuration")) { failed = 1; break; }
        status = ft_acquisition_d3d11_install_configuration(connection, consumer);
        if (status == FT_STATUS_OK) continue;
        if (status == FT_STATUS_CLOSED) break;
        if (status != FT_STATUS_EMPTY && status != FT_STATUS_STALE) {
          fprintf(stderr, "D3D11 configuration failed with status %d\n", status);
          failed = 1;
          break;
        }
      }
    } else if (status == FT_STATUS_HOLDING_LIMIT) {
      interest = FT_WAIT_CAPACITY;
    } else if (status != FT_STATUS_EMPTY && status != FT_STATUS_MISS) {
      fprintf(stderr, "D3D11 acquisition failed with status %d\n", status);
      failed = 1;
      break;
    }
    ft_acquisition_events after = {0};
    status = ft_acquisition_wait(consumer, &before, interest, cancellation, 16 * 1000 * 1000, &after);
    if (status == FT_STATUS_CLOSED || status == FT_STATUS_CANCELLED) break;
    if (status != FT_STATUS_OK && status != FT_STATUS_TIMEOUT) {
      fprintf(stderr, "D3D11 acquisition wait failed with status %d\n", status);
      failed = 1;
      break;
    }
  }
  /* A timeout reports failure; it never declares GPU work complete. */
  if (!drain(&state, 5000)) {
    fprintf(stderr, "viewer GPU work did not complete\n");
    failed = 1;
  }
  if (options.max_frames > 0 && presented != (uint64_t)options.max_frames) {
    fprintf(stderr, "expected %ld frames, presented %" PRIu64 "%s\n", options.max_frames, presented,
            producer_lost ? " (producer lost)" : "");
    failed = 1;
  }
  printf("presented_frames=%" PRIu64 "\n", presented);

cleanup:
  ft_acquisition_cancellation_destroy(&cancellation);
  ft_acquisition_release_timeline_destroy(&timeline);
  ft_acquisition_consumer_destroy(&consumer);
  ft_acquisition_d3d11_connection_destroy(&connection);
  ft_local_connection_destroy(&local);
  for (int i = 0; i < MAX_CACHED_TEXTURES; i++) {
    RELEASE(state.textures[i].view);
    RELEASE(state.textures[i].texture);
  }
  RELEASE(state.readiness);
  RELEASE(state.release_fence);
  RELEASE(state.sampler);
  RELEASE(state.pixel_shader);
  RELEASE(state.vertex_shader);
  RELEASE(state.target);
  RELEASE(state.swap_chain);
  RELEASE(state.context4);
  RELEASE(state.context);
  RELEASE(state.device5);
  RELEASE(state.device1);
  RELEASE(state.device);
  RELEASE(state.factory);
  if (state.window != NULL) DestroyWindow(state.window);
  return failed ? 1 : 0;
}
