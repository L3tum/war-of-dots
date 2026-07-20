interface ReplayInfo {
  game_id: string;
  winner: string;
  players: string[];
  start_time: number;
  end_time: number;
  duration: number;
}

export class SpectatorReplay {
  private canvas: HTMLCanvasElement;

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
  }

  async start() {
    this.renderReplayList();
  }

  private renderReplayList() {
    document.body.innerHTML = '';

    const container = document.createElement('div');
    container.style.padding = '20px';
    container.style.fontFamily = 'sans-serif';
    container.style.backgroundColor = '#1a1a2e';
    container.style.color = '#eaeaea';
    container.style.minHeight = '100vh';

    const title = document.createElement('h1');
    title.textContent = 'Game Replays';
    title.style.fontSize = '24px';
    container.appendChild(title);

    const listContainer = document.createElement('div');
    listContainer.style.marginTop = '20px';
    container.appendChild(listContainer);

    fetch('/replays')
      .then((res) => res.json())
      .then((replays: ReplayInfo[]) => {
        if (replays.length === 0) {
          listContainer.textContent = 'No replays available yet.';
          return;
        }

        replays.forEach((replay) => {
          const item = document.createElement('div');
          item.style.padding = '10px';
          item.style.borderBottom = '1px solid #333';
          item.style.cursor = 'pointer';
          item.style.transition = 'background-color 0.2s';

          const date = new Date(replay.end_time * 1000);
          const dateStr = date.toLocaleString();
          const winner = replay.winner;
          const durationStr = `${Math.floor(replay.duration / 60)}m ${replay.duration % 60}s`;

          item.innerHTML = `
            <strong>${dateStr}</strong> — Winner: ${winner}<br>
            Players: ${replay.players.join(', ')}<br>
            Duration: ${durationStr}
          `;

          item.addEventListener('mouseenter', () => {
            item.style.backgroundColor = '#2a2a4e';
          });
          item.addEventListener('mouseleave', () => {
            item.style.backgroundColor = '#1a1a2e';
          });

          item.addEventListener('click', () => {
            window.location.href = `/replay?game_id=${replay.game_id}`;
          });

          listContainer.appendChild(item);
        });
      })
      .catch((err) => {
        listContainer.textContent = 'Failed to load replays: ' + err;
      });

    document.body.appendChild(container);
  }
}
