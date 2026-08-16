import React from 'react';
import { Files, History, ListTree, Search } from 'lucide-react';
import { useLayout, SidebarView } from '../store/layout';

const items: { view: SidebarView; icon: React.ReactNode; title: string }[] = [
  { view: 'explorer', icon: <Files size={19} />, title: 'Explorer' },
  { view: 'search', icon: <Search size={19} />, title: 'Search' },
  { view: 'outline', icon: <ListTree size={19} />, title: 'Outline' },
  { view: 'history', icon: <History size={19} />, title: 'Checkpoints' },
];

export const ActivityBar: React.FC = () => {
  const sidebarView = useLayout((s) => s.sidebarView);
  const sidebarOpen = useLayout((s) => s.sidebarOpen);
  const setSidebarView = useLayout((s) => s.setSidebarView);

  return (
    <nav className="activity-bar">
      {items.map((item) => (
        <button
          key={item.view}
          className={`act-btn ${sidebarView === item.view && sidebarOpen ? 'active' : ''}`}
          title={item.title}
          onClick={() => setSidebarView(item.view)}
        >
          {item.icon}
        </button>
      ))}
    </nav>
  );
};
