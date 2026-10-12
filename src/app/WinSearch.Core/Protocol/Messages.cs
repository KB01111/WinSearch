using System.Text.Json.Serialization;

namespace WinSearch.Core.Protocol;

// ---------------------------------------------------------------------------
// Client -> engine
// ---------------------------------------------------------------------------

[JsonPolymorphic(TypeDiscriminatorPropertyName = "type")]
[JsonDerivedType(typeof(HelloMessage), "hello")]
[JsonDerivedType(typeof(PingMessage), "ping")]
[JsonDerivedType(typeof(SearchMessage), "search")]
[JsonDerivedType(typeof(CancelMessage), "cancel")]
[JsonDerivedType(typeof(GetStatusMessage), "get_status")]
public abstract class ClientMessage;

public sealed class HelloMessage : ClientMessage
{
    public uint ProtocolVersion { get; set; } = ProtocolJson.Version;
    public string Client { get; set; } = "";
    public string ClientVersion { get; set; } = "";
}

public sealed class PingMessage : ClientMessage
{
    public ulong Id { get; set; }
}

public sealed class SearchMessage : ClientMessage
{
    public ulong Id { get; set; }
    public SearchQuery Query { get; set; } = new();
}

public sealed class CancelMessage : ClientMessage
{
    public ulong Id { get; set; }
}

public sealed class GetStatusMessage : ClientMessage
{
    public ulong Id { get; set; }
}

// ---------------------------------------------------------------------------
// Engine -> client
// ---------------------------------------------------------------------------

[JsonPolymorphic(TypeDiscriminatorPropertyName = "type")]
[JsonDerivedType(typeof(HelloAckMessage), "hello_ack")]
[JsonDerivedType(typeof(PongMessage), "pong")]
[JsonDerivedType(typeof(ResultsMessage), "results")]
[JsonDerivedType(typeof(StatusMessage), "status")]
[JsonDerivedType(typeof(ErrorMessage), "error")]
public abstract class ServerMessage;

public sealed class HelloAckMessage : ServerMessage
{
    public uint ProtocolVersion { get; set; }
    public string EngineVersion { get; set; } = "";
    public List<string> Features { get; set; } = new();
}

public sealed class PongMessage : ServerMessage
{
    public ulong Id { get; set; }
}

public sealed class ResultsMessage : ServerMessage
{
    public ulong Id { get; set; }
    public List<SearchResult> Batch { get; set; } = new();
    public bool Done { get; set; }
}

public sealed class StatusMessage : ServerMessage
{
    /// <summary>Set when answering GetStatus; null for pushed updates.</summary>
    public ulong? Id { get; set; }
    public EngineStatus Status { get; set; } = new();
}

public sealed class ErrorMessage : ServerMessage
{
    public ulong? Id { get; set; }
    public ErrorCode Code { get; set; }
    public string Message { get; set; } = "";
}

// ---------------------------------------------------------------------------
// Shared models
// ---------------------------------------------------------------------------

public enum SearchMode
{
    Auto,
    Apps,
    Files,
    Code,
}

public enum ResultKind
{
    Application,
    File,
    Folder,
    Image,
    Screenshot,
    Document,
    Audio,
    Video,
    Code,
    Calculator,
    Settings,
    Action,
}

public enum ErrorCode
{
    ProtocolVersionMismatch,
    InvalidRequest,
    Internal,
    NotReady,
    Cancelled,
    Unsupported,
}

public enum EngineRunState
{
    Idle,
    Indexing,
    Paused,
    Error,
}

public sealed class SearchQuery
{
    public string Text { get; set; } = "";
    public SearchMode Mode { get; set; } = SearchMode.Auto;
    public uint Limit { get; set; } = 20;
    public bool IncludeSemantic { get; set; }
    public List<ResultKind> Categories { get; set; } = new();
}

[JsonPolymorphic(TypeDiscriminatorPropertyName = "kind")]
[JsonDerivedType(typeof(PageLocation), "page")]
[JsonDerivedType(typeof(TimestampLocation), "timestamp")]
[JsonDerivedType(typeof(LineLocation), "line")]
public abstract class MatchLocation;

public sealed class PageLocation : MatchLocation
{
    public uint Page { get; set; }
}

public sealed class TimestampLocation : MatchLocation
{
    public double Seconds { get; set; }
    public double EndSeconds { get; set; }
}

public sealed class LineLocation : MatchLocation
{
    public uint Line { get; set; }
}

public sealed class SearchResult
{
    public string Id { get; set; } = "";
    public ResultKind Kind { get; set; }
    public string Title { get; set; } = "";
    public string Subtitle { get; set; } = "";
    public string MatchExplanation { get; set; } = "";
    public double Score { get; set; }
    public string? Path { get; set; }
    public MatchLocation? Location { get; set; }
    public string? ThumbnailPath { get; set; }
}

public sealed class EngineStatus
{
    public EngineRunState State { get; set; }
    public string CurrentActivity { get; set; } = "";
    public ulong FilesDiscovered { get; set; }
    public ulong FilesIndexed { get; set; }
    public ulong SegmentsIndexed { get; set; }
    public ulong QueueDepth { get; set; }
    public bool ModelLoaded { get; set; }
    public ulong IndexSizeBytes { get; set; }
}

/// <summary>One streamed chunk of search results (client-side only).</summary>
public sealed class SearchBatch
{
    public required IReadOnlyList<SearchResult> Results { get; init; }
    public bool Done { get; init; }
}

/// <summary>Raised when the engine rejects a request.</summary>
public sealed class EngineException : Exception
{
    public ErrorCode Code { get; }

    public EngineException(ErrorCode code, string message) : base(message)
    {
        Code = code;
    }
}
