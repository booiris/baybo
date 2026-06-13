import { Routes, Route } from 'react-router-dom';
import { Layout } from './components/Layout';
import { HomePage } from './pages/HomePage';
import { BenchPage } from './pages/BenchPage';
import { RunPage } from './pages/RunPage';
import { ItemPage } from './pages/ItemPage';
import { SearchPage } from './pages/SearchPage';

export default function App() {
  return (
    <Routes>
      <Route element={<Layout />}>
        <Route path="/" element={<HomePage />} />
        <Route path="/bench/:benchId" element={<BenchPage />} />
        <Route path="/bench/:benchId/run/:runKey" element={<RunPage />} />
        <Route path="/bench/:benchId/run/:runKey/item/:itemId" element={<ItemPage />} />
        <Route path="/search" element={<SearchPage />} />
      </Route>
    </Routes>
  );
}
