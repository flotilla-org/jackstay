/* Standalone C host/consumer exercise. No SDL, daemon, capture permissions or
 * private Jackstay headers. exec precedes all grant/map creation; the child
 * connects itself, so kernel peer identity names the actual consumer process. */
#undef NDEBUG
#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include "capture_transfer.h"

static void send_byte(int fd, char value) { assert(write(fd, &value, 1) == 1); }
static void expect_byte(int fd, char value) {
    char got = 0;
    assert(read(fd, &got, 1) == 1 && got == value);
}
static struct sockaddr_un address(const char *path) {
    struct sockaddr_un addr = {0};
    addr.sun_family = AF_UNIX;
    assert(strlen(path) < sizeof(addr.sun_path));
    strcpy(addr.sun_path, path);
#ifdef __APPLE__
    addr.sun_len = (uint8_t)(offsetof(struct sockaddr_un, sun_path) + strlen(path) + 1);
#endif
    return addr;
}
static void check_frame(ft_acquired_frame *frame, const char *expected, size_t size, uint32_t width) {
    const uint8_t *bytes = NULL;
    size_t len = 0;
    ft_acquired_frame_descriptor descriptor = {0};
    assert(ft_acquired_frame_bytes(frame, &bytes, &len) == FT_STATUS_OK);
    assert(len == size && memcmp(bytes, expected, size) == 0);
    assert(ft_acquired_frame_describe(frame, &descriptor) == FT_STATUS_OK);
    assert(descriptor.width == width && descriptor.height == 1 && descriptor.stride == width * 4);
}
static int consumer_main(const char *path, int commands, int replies, int crash) {
    int32_t fd = socket(AF_UNIX, SOCK_STREAM, 0);
    assert(fd >= 0);
    struct sockaddr_un addr = address(path);
    assert(connect(fd, (struct sockaddr *)&addr, sizeof(addr)) == 0);
    ft_cpu_acquisition_connection *connection = NULL;
    ft_acquisition_consumer *consumer = NULL;
    assert(ft_acquisition_cpu_connection_create(&fd, &connection) == FT_STATUS_OK && fd == -1);
    assert(ft_acquisition_cpu_attach(connection, 2, &consumer) == FT_STATUS_OK);
    send_byte(replies, 'A');
    expect_byte(commands, 'P');
    ft_acquired_frame *old = NULL, *current = NULL;
    ft_acquisition_range range = {0};
    assert(ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &old, &range) == FT_STATUS_OK);
    check_frame(old, "held", 4, 1);
    send_byte(replies, 'H');
    expect_byte(commands, 'R');
    assert(ft_acquisition_cpu_install_configuration(connection, consumer) == FT_STATUS_OK);
    assert(ft_acquisition_cpu_install_configuration(connection, consumer) == FT_STATUS_EMPTY);
    assert(ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &current, &range) == FT_STATUS_OK);
    check_frame(old, "held", 4, 1);
    check_frame(current, "newframe", 8, 2);
    send_byte(replies, 'N');
    expect_byte(commands, 'C');
    ft_acquired_frame *next = NULL;
    assert(ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &next, &range) == FT_STATUS_CLOSED);
    if (crash) _exit(0); /* Deliberately abandon both claims and all map owners. */
    ft_acquisition_cpu_connection_destroy(&connection);
    ft_acquisition_consumer_destroy(&consumer);
    check_frame(old, "held", 4, 1);
    check_frame(current, "newframe", 8, 2);
    assert(ft_acquired_frame_release(&old) == FT_STATUS_OK && old == NULL);
    assert(ft_acquired_frame_release(&current) == FT_STATUS_OK && current == NULL);
    return 0;
}
static void publish(ft_cpu_producer *producer, const char *bytes, uint32_t width) {
    ft_acquired_frame_descriptor descriptor = {0};
    descriptor.width = width;
    descriptor.height = 1;
    descriptor.stride = width * 4;
    descriptor.pixel_format = FT_PIXEL_FORMAT_BGRA8_UNORM;
    uint64_t cursor = 0;
    assert(ft_cpu_producer_publish(producer, &descriptor, (const uint8_t *)bytes, width * 4, &cursor) == FT_STATUS_OK);
    assert(cursor != 0);
}
static void host_main(const char *executable, int crash) {
    char directory[] = "/tmp/js-c-XXXXXX";
    assert(mkdtemp(directory) != NULL);
    char path[80];
    assert(snprintf(path, sizeof(path), "%s/setup", directory) < (int)sizeof(path));
    struct sockaddr_un addr = address(path);
    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    assert(listener >= 0);
    assert(bind(listener, (struct sockaddr *)&addr, sizeof(addr)) == 0);
    assert(listen(listener, 1) == 0);
    int commands[2], replies[2];
    assert(pipe(commands) == 0 && pipe(replies) == 0);
    char command_fd[32], reply_fd[32];
    snprintf(command_fd, sizeof(command_fd), "%d", commands[0]);
    snprintf(reply_fd, sizeof(reply_fd), "%d", replies[1]);
    pid_t child = fork();
    assert(child >= 0);
    if (child == 0) {
        close(listener);
        close(commands[1]);
        close(replies[0]);
        execl(executable, executable, "--child", path, command_fd, reply_fd, crash ? "1" : "0", (char *)NULL);
        _exit(127);
    }
    close(commands[0]);
    close(replies[1]);
    ft_cpu_producer *producer = NULL;
    ft_cpu_producer_config config = {6, 2, 1, 2, 4, 1024 * 1024, 5000000000ULL};
    assert(ft_cpu_producer_create(&config, &producer) == FT_STATUS_OK);
    int32_t fd = accept(listener, NULL, NULL);
    assert(fd >= 0);
    close(listener);
    assert(unlink(path) == 0 && rmdir(directory) == 0);
    ft_cpu_setup_server *server = NULL;
    assert(ft_cpu_producer_serve(producer, &fd, &server) == FT_STATUS_OK && fd == -1);
    expect_byte(replies[0], 'A');
    publish(producer, "held", 1);
    send_byte(commands[1], 'P');
    expect_byte(replies[0], 'H');
    ft_cpu_reconfiguration replacement = {0};
    assert(ft_cpu_producer_reconfigure(producer, 8, &replacement) == FT_STATUS_OK);
    publish(producer, "newframe", 2);
    send_byte(commands[1], 'R');
    expect_byte(replies[0], 'N');
    assert(ft_cpu_setup_server_destroy(&server) == FT_STATUS_CANCELLED && server == NULL);
    assert(ft_cpu_producer_destroy(&producer) == FT_STATUS_DRAINING && producer != NULL);
    send_byte(commands[1], 'C');
    int status = 0;
    assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    close(commands[1]);
    close(replies[0]);
    /* Kernel process-death notification can arrive asynchronously after waitpid. */
    for (int attempt = 0; attempt < 500; ++attempt) {
        ft_status result = ft_cpu_producer_destroy(&producer);
        if (result == FT_STATUS_OK) { assert(producer == NULL); return; }
        assert(result == FT_STATUS_DRAINING);
        struct timespec delay = {0, 10000000};
        nanosleep(&delay, NULL);
    }
    assert(!"producer did not retire after consumer exit");
}
/* Exercise the actual SDL CLI against a producer in a separate process. The
 * retained frame is available before attach; no scheduling sleeps are needed. */
static void viewer_main(const char *viewer) {
    char directory[] = "/tmp/js-viewer-XXXXXX";
    assert(mkdtemp(directory) != NULL);
    char path[80];
    assert(snprintf(path, sizeof(path), "%s/setup", directory) < (int)sizeof(path));
    struct sockaddr_un addr = address(path);
    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    assert(listener >= 0);
    assert(bind(listener, (struct sockaddr *)&addr, sizeof(addr)) == 0);
    assert(listen(listener, 1) == 0);
    pid_t child = fork();
    assert(child >= 0);
    if (child == 0) {
        close(listener);
        execl(viewer, viewer, "--cpu-socket", path, "--frames", "3", (char *)NULL);
        _exit(127);
    }
    ft_cpu_producer *producer = NULL;
    ft_cpu_producer_config config = {4, 1, 1, 2, 8, 1024 * 1024, 5000000000ULL};
    assert(ft_cpu_producer_create(&config, &producer) == FT_STATUS_OK);
    publish(producer, "SDLframe", 2);
    int32_t fd = accept(listener, NULL, NULL);
    assert(fd >= 0);
    close(listener);
    assert(unlink(path) == 0 && rmdir(directory) == 0);
    ft_cpu_setup_server *server = NULL;
    assert(ft_cpu_producer_serve(producer, &fd, &server) == FT_STATUS_OK && fd == -1);
    int status = 0;
    assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    ft_status stopped = ft_cpu_setup_server_destroy(&server);
    assert((stopped == FT_STATUS_OK || stopped == FT_STATUS_CANCELLED) && server == NULL);
    for (int attempt = 0; attempt < 500; ++attempt) {
        ft_status result = ft_cpu_producer_destroy(&producer);
        if (result == FT_STATUS_OK) { assert(producer == NULL); return; }
        assert(result == FT_STATUS_DRAINING);
        struct timespec delay = {0, 10000000};
        nanosleep(&delay, NULL);
    }
    assert(!"producer did not retire after SDL viewer exit");
}
int main(int argc, char **argv) {
    alarm(15);
    assert(ft_abi_version() == FT_ABI_VERSION);
    if (argc == 6 && strcmp(argv[1], "--child") == 0)
        return consumer_main(argv[2], atoi(argv[3]), atoi(argv[4]), atoi(argv[5]));
    if (argc == 3 && strcmp(argv[1], "--viewer") == 0) {
        viewer_main(argv[2]);
        return 0;
    }
    assert(argc == 1);
    host_main(argv[0], 0);
    host_main(argv[0], 1);
    puts("cross-process C setup: replacement, cancellation, release and crash cleanup passed");
    return 0;
}
