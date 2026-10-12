using System.Collections.ObjectModel;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using WinSearch.Core.Ipc;
using WinSearch.Core.Protocol;

namespace WinSearch.ViewModels;

/// <summary>
/// State and behaviour of the search overlay: progressive search with
/// debounce, cancellation of superseded queries, and stable result updates.
/// Exact matches stream in first; semantic batches append later without
/// disturbing the user's selection (the view keeps selection by result id).
/// </summary>
public sealed partial class MainOverlayViewModel : ObservableObject, IAsyncDisposable
{
    private const int DebounceMs = 120;

    private readonly EngineClient _client;
    private readonly SynchronizationContext? _ui;
    private CancellationTokenSource? _searchCts;
    private Timer? _debounceTimer;
    private long _searchSequence;
    private string _pendingQuery = "";
    private bool _disposed;

    public MainOverlayViewModel(EngineClient client)
    {
        _client = client;
        _ui = SynchronizationContext.Current;
        _client.StatusChanged += OnStatusChanged;
    }

    public ObservableCollection<ResultItemViewModel> Results { get; } = new();

    [ObservableProperty]
    private string _queryText = "";

    [ObservableProperty]
    private string _statusText = "";

    [ObservableProperty]
    private string _modeLabel = "All";

    [ObservableProperty]
    private bool _isSearching;

    /// <summary>True when semantic retrieval is running on top of exact results.</summary>
    [ObservableProperty]
    private bool _isSemanticPending;

    partial void OnQueryTextChanged(string value)
    {
        _pendingQuery = value;
        _debounceTimer?.Dispose();
        _debounceTimer = new Timer(
            static state => ((MainOverlayViewModel)state!).OnDebounceElapsed(),
            this,
            DebounceMs,
            Timeout.Infinite);
    }

    private void OnDebounceElapsed()
    {
        _ = RunSearchAsync(_pendingQuery);
    }

    /// <summary>Runs immediately (Enter / mode switch), bypassing the debounce.</summary>
    [RelayCommand]
    private Task SearchNowAsync() => RunSearchAsync(QueryText);

    private async Task RunSearchAsync(string query)
    {
        if (_disposed)
        {
            return;
        }

        _searchCts?.Cancel();
        _searchCts?.Dispose();
        _searchCts = new CancellationTokenSource();
        var ct = _searchCts.Token;
        var sequence = Interlocked.Increment(ref _searchSequence);

        if (string.IsNullOrWhiteSpace(query))
        {
            Dispatch(() =>
            {
                Results.Clear();
                IsSearching = false;
                IsSemanticPending = false;
                StatusText = "";
            });
            return;
        }

        Dispatch(() =>
        {
            IsSearching = true;
            StatusText = "Searching…";
        });

        var mode = query.StartsWith("code:", StringComparison.OrdinalIgnoreCase)
            ? SearchMode.Code
            : SearchMode.Auto;
        var effectiveText = mode == SearchMode.Code ? query["code:".Length..].Trim() : query;
        Dispatch(() => ModeLabel = mode == SearchMode.Code ? "Code" : "All");

        var searchQuery = new SearchQuery
        {
            Text = effectiveText,
            Mode = mode,
            Limit = 20,
            // Semantic runs after exact results; enabled from Phase 3 onward.
            IncludeSemantic = false,
        };

        try
        {
            var seen = 0;
            await foreach (var batch in _client.SearchAsync(searchQuery, ct))
            {
                if (sequence != Interlocked.Read(ref _searchSequence))
                {
                    return; // superseded by a newer query
                }

                var items = batch.Results.Select(r => new ResultItemViewModel(r)).ToList();
                seen += items.Count;
                Dispatch(() =>
                {
                    foreach (var item in items)
                    {
                        Results.Add(item);
                    }
                    StatusText = seen == 0 ? "No matches" : $"{seen} result{(seen == 1 ? "" : "s")}";
                    IsSearching = false;
                });
            }
        }
        catch (OperationCanceledException)
        {
            // Superseded query: nothing to report.
        }
        catch (EngineException ex)
        {
            Dispatch(() =>
            {
                IsSearching = false;
                StatusText = ex.Code == ErrorCode.NotReady ? "Search is not ready yet" : ex.Message;
            });
        }
        catch (IOException)
        {
            Dispatch(() =>
            {
                IsSearching = false;
                StatusText = "Lost connection to the search engine";
            });
        }
    }

    private void OnStatusChanged(object? sender, EngineStatus status)
    {
        Dispatch(() =>
        {
            if (!IsSearching)
            {
                StatusText = status.State switch
                {
                    EngineRunState.Indexing =>
                        $"Indexing… {status.FilesIndexed:N0} of {status.FilesDiscovered:N0} files",
                    EngineRunState.Paused => "Indexing paused",
                    EngineRunState.Error => "Indexing error — see logs",
                    _ when status.FilesIndexed > 0 => $"{status.FilesIndexed:N0} files indexed",
                    _ => "",
                };
            }
        });
    }

    public Task RefreshStatusAsync(CancellationToken cancellationToken = default)
        => _client.GetStatusAsync(cancellationToken);

    private void Dispatch(Action action)
    {
        if (_ui is null)
        {
            action();
        }
        else
        {
            _ui.Post(_ => action(), null);
        }
    }

    public ValueTask DisposeAsync()
    {
        if (_disposed)
        {
            return ValueTask.CompletedTask;
        }
        _disposed = true;
        _client.StatusChanged -= OnStatusChanged;
        _debounceTimer?.Dispose();
        _searchCts?.Cancel();
        _searchCts?.Dispose();
        return ValueTask.CompletedTask;
    }
}
