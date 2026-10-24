import java.net.InetSocketAddress;
import java.time.Duration;
import java.util.concurrent.*;
import org.facet.vox.*;
import org.facet.vox.generated.*;

/** Exercise immediate input without prompt matching, sleeps or automatic replay of success. */
public final class InputReadinessProbe {
    private static final String COMMAND = "$global:__vox_probe_count = 1 + [int]$global:__vox_probe_count; Write-Output ([string]::Concat('VOX_JAVA_', 'READY_λ:', $global:__vox_probe_count))\r";

    private static boolean attempt(TerminalClient client, String session) throws Exception {
        VoxResult<TerminalInputResult, TerminalError> result = TerminalProbe.await(client.sendText(
                new TerminalTextInput(session, COMMAND, 4, "repeated-id")));
        if (result.isSuccess()) return true;
        TerminalProbe.check(result.isApplicationError(), "input rejection must be typed");
        TerminalError error = result.applicationError();
        TerminalProbe.check(error.code() == TerminalErrorCode.INVALID_REQUEST && error.retryable()
                && error.message().contains("input was not forwarded"), "explicit safe-to-retry startup rejection: " + error);
        return false;
    }

    public static void main(String[] args) throws Exception {
        InetSocketAddress endpoint = new InetSocketAddress("127.0.0.1", Integer.parseInt(args[0]));
        ExecutorService drivers = Executors.newCachedThreadPool();
        try (VoxConnection connection = VoxConnection.connect(endpoint, ConnectionOptions.defaults())) {
            TerminalClient client = new TerminalClient(TerminalProbe.open(connection, drivers));
            String session = TerminalProbe.success(TerminalProbe.await(client.connect(
                    TerminalProbe.request("rust-cpu-fontdue")))).sessionId();
            // No intervening readiness polling, frame wait, foreign connection or sleep.
            boolean accepted = attempt(client, session);
            int rejected = accepted ? 0 : 1;
            // One in-flight RPC at a time; no frame/render delay hides the gate transition.
            long admissionDeadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(15);
            while (!accepted && System.nanoTime() < admissionDeadline) {
                accepted = attempt(client, session);
                if (!accepted) rejected++;
            }
            TerminalProbe.check(accepted, "bounded startup admission");
            VoxChannels.Pair<TerminalFrameEvent> frames = VoxChannels.channel(TerminalFrameEvent.ADAPTER);
            CompletableFuture<VoxResult<TerminalOperationResult, TerminalError>> subscription = client.subscribeFrames(
                    new TerminalSubscribeRequest(session, 0, TerminalProbe.MAX_BYTES, 3, "readiness-stream"), frames.tx());
            try (VoxRx<TerminalFrameEvent> rx = frames.rx()) {
                boolean repeated = false;
                boolean complete = false;
                long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(20);
                while (System.nanoTime() < deadline) {
                    TerminalFrameEvent frame;
                    try { frame = rx.receive(Duration.ofSeconds(15)); }
                    catch (TimeoutException failure) {
                        System.err.println("accepted=" + accepted + ", repeated=" + repeated + ", rejected=" + rejected);
                        System.err.println("TIMEOUT_CONTENT=" + TerminalProbe.success(TerminalProbe.await(client.getContent(
                                new TerminalContentRequest(session, 0, 8192, 5)))).text());
                        throw failure;
                    }
                    TerminalProbe.png(frame, session);
                    String text = TerminalProbe.success(TerminalProbe.await(client.getContent(
                            new TerminalContentRequest(session, 0, 8192, 5)))).text();
                    long count = text.lines().map(String::trim)
                            .filter(line -> line.matches("VOX_JAVA_READY_λ:[0-9]+"))
                            .mapToLong(line -> Long.parseLong(line.substring(line.indexOf(':') + 1)))
                            .max().orElse(0);
                    TerminalProbe.check(count <= 2, "rejected sends must not queue or replay: " + text);
                    if (count == 1 && !repeated) {
                        TerminalProbe.check(accepted, "output requires accepted input");
                        TerminalProbe.check(attempt(client, session), "ready session admits repeated same-id send");
                        repeated = true;
                    } else if (count == 2) {
                        TerminalProbe.check(repeated, "two outputs require two accepted operations");
                        complete = true;
                        break;
                    }
                }
                TerminalProbe.check(complete, "exactly two accepted inputs must execute and stream");
            }
            TerminalProbe.success(TerminalProbe.await(subscription));
            TerminalProbe.success(TerminalProbe.await(client.disconnect(
                    new TerminalDisconnectRequest(session, "readiness complete", 7))));
            TerminalProbe.missing(TerminalProbe.await(client.sendText(
                    new TerminalTextInput(session, COMMAND, 4, "repeated-id"))));
            System.out.println("PASS immediate input: explicit rejections=" + rejected + "; two accepted same-id sends executed exactly twice");
            System.out.println("PASS CPU stream, receiver cancellation and post-close input rejection");
        } finally {
            drivers.shutdownNow();
            TerminalProbe.check(drivers.awaitTermination(5, TimeUnit.SECONDS), "driver cleanup");
        }
    }
}
